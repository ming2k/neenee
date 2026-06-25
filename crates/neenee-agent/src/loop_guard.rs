//! Deterministic, non-terminating defence against read loops.
//!
//! A model occasionally gets stuck issuing the *same* read over and over —
//! re-reading one file (or thrashing between two pages of it) without making
//! progress. The agentic loop is uncapped by design (ADR-0009) and the harness
//! deliberately keeps no hard equality guard (the old `guard_repeated_call` and
//! the ADR-0030 review nudge were removed in favour of the model-driven `abort`
//! tool), so nothing automatically breaks the trajectory — and a model that is
//! looping is, by definition, not noticing it should `abort`. This module fills
//! that gap without resurrecting a hard cap.
//!
//! ## Why this is deterministic (not a semantic judgement)
//!
//! Identical read arguments return byte-for-byte identical content. So "the
//! model keeps re-issuing the same read" is a *provable* waste — it needs no LLM
//! to adjudicate, unlike the fuzzier "lots of reads but each different" case
//! that [`crate::session_review`] handles. Detection is pure string bookkeeping,
//! which is why it is free, instant, and has no false positives on legitimate
//! work: real research reads *different* things, so its signatures never repeat.
//!
//! ## Why a frequency window (not a consecutive counter)
//!
//! A naive "same signature N rounds in a row" counter misses the oscillation
//! pattern `A B A B A B` — the exact shape a two-page thrash produces — because
//! no signature is ever consecutive. Instead we keep a sliding window of the
//! last [`WINDOW`] read-round signatures and fire when any signature occurs
//! [`THRESHOLD`] times *within* the window. That catches `A A A`, `A B A B A`,
//! and anything in between, while leaving genuine forward paging (`A B C D E`,
//! all distinct) untouched.
//!
//! ## What firing does
//!
//! It returns a nudge string. The caller appends it as a hidden user message
//! ([`neenee_core::InjectionKind::LoopReviewNudge`]) before the next model
//! request, injecting information the self-reinforcing context lacked: *you have
//! repeated this exact read, it is unchanged, change course.* The turn keeps
//! running — `Esc`, the opt-in `hard_stop_rounds`, and `abort` remain the hard
//! backstops. A one-shot-per-signature latch keeps it from spamming; it escalates
//! to a sterner wording once if the loop persists to [`ESCALATE_AT`].

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// Sliding-window size: how many recent read-rounds are considered when judging
/// whether a signature is recurring. Large enough to span a `A B A B` thrash,
/// small enough that an old, since-abandoned read ages out and stops counting.
pub const WINDOW: usize = 8;

/// Occurrences of one signature within the window that constitute a loop. Two
/// could be a legitimate "read, glance away, re-read"; three in an 8-round window
/// is not plausibly productive.
pub const THRESHOLD: u32 = 3;

/// If a signature reaches this many occurrences after the first nudge was already
/// sent, escalate to a sterner, final nudge. Beyond this we stay silent and let
/// the hard backstops (`hard_stop_rounds`, `abort`, `Esc`) take over rather than
/// nag every round.
pub const ESCALATE_AT: u32 = 6;

/// How a completed tool round looks to the guard. Computed by the caller (which
/// owns tool-access classification) and fed to [`ReadLoopGuard::observe`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RoundClass {
    /// An all-read round, reduced to its canonical signature (see
    /// [`read_signature`]). A repeating signature is what the guard watches for.
    Read(String),
    /// Anything that is not a pure read round — a write, an execute, a mixed
    /// round, or no tool call at all. Real progress, so it clears the window: a
    /// loop that resumes afterwards is a *fresh* loop, re-armed from scratch.
    #[default]
    Progress,
}

/// Per-turn read-loop detector. Cheap value type; one lives in `TurnState` and is
/// dropped when the turn ends, so loop state never leaks across turns.
#[derive(Default)]
pub struct ReadLoopGuard {
    /// Signatures of the last [`WINDOW`] read rounds, oldest at the front.
    window: VecDeque<String>,
    /// Per-signature latch: how many nudges this signature has already drawn in
    /// its current streak. Cleared for a signature once it ages out of the
    /// window (its streak is over), so a later recurrence can nudge again.
    nudges_sent: HashMap<String, u8>,
}

impl ReadLoopGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one completed tool round and return a nudge prompt iff this round
    /// pushed a read signature to (or past) the loop threshold and the latch
    /// admits firing. `None` the rest of the time — including every non-read
    /// round, which also resets the window.
    pub fn observe(&mut self, round: RoundClass) -> Option<String> {
        let signature = match round {
            RoundClass::Progress => {
                self.reset();
                return None;
            }
            RoundClass::Read(signature) => signature,
        };

        self.push(signature.clone());
        let count = self.count(&signature);
        if count < THRESHOLD {
            return None;
        }

        // Latch by level: fire once when the loop is first confirmed, and once
        // more if it persists to the escalation point. Never more than twice per
        // signature streak.
        let already_sent = self.nudges_sent.get(&signature).copied().unwrap_or(0);
        let level = match already_sent {
            0 => 1,
            1 if count >= ESCALATE_AT => 2,
            _ => return None,
        };
        self.nudges_sent.insert(signature.clone(), level);
        Some(build_nudge(&signature, count, level))
    }

    fn push(&mut self, signature: String) {
        self.window.push_back(signature);
        while self.window.len() > WINDOW {
            let evicted = self.window.pop_front().expect("non-empty");
            // The evicted signature's streak is over once it no longer appears in
            // the window: drop its latch so a future recurrence re-arms cleanly.
            if !self.window.contains(&evicted) {
                self.nudges_sent.remove(&evicted);
            }
        }
    }

    fn count(&self, signature: &str) -> u32 {
        self.window.iter().filter(|s| *s == signature).count() as u32
    }

    fn reset(&mut self) {
        self.window.clear();
        self.nudges_sent.clear();
    }
}

/// Canonical signature of an all-read round: the per-call signatures, sorted and
/// joined so a round's identity is independent of the order the model happened to
/// emit its parallel reads in.
pub fn read_signature<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut parts: Vec<String> = calls
        .into_iter()
        .map(|(name, args)| call_signature(name, args))
        .collect();
    parts.sort();
    parts.join(" + ")
}

/// Signature of one read call. For a file-addressed read we key on
/// `name|path|offset|limit` with pagination defaults normalised (`offset`→1,
/// `limit`→0) so the model cannot dodge the guard by toggling a default, and so a
/// read of a *different* line range is correctly a *different* signature (genuine
/// paging never trips). For a query-shaped read (e.g. `grep`) we fall back to the
/// raw arguments so distinct queries stay distinct.
fn call_signature(name: &str, args: &str) -> String {
    let value: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let path = ["path", "file_path", "file", "filename"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str));
    match path {
        Some(path) if !has_query_arg(&value) => {
            let offset = value
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1);
            let limit = value.get("limit").and_then(Value::as_u64).unwrap_or(0);
            format!("{name}|{path}|{offset}|{limit}")
        }
        _ => format!("{name}|{}", args.trim()),
    }
}

/// Whether the arguments carry a query/content payload that distinguishes two
/// calls on the same path (so they must not be collapsed to a path-only key).
fn has_query_arg(value: &Value) -> bool {
    ["pattern", "query", "command", "cmd", "url"]
        .iter()
        .any(|key| value.get(*key).is_some())
}

/// Build the nudge text. Level 1 is the first, informative break; level 2 is the
/// sterner escalation when the loop persists. The wording is a fixed template —
/// the *information* (you repeated this, it is unchanged, change course) is what
/// breaks the anchor, not eloquence, so it needs no model call to compose.
fn build_nudge(signature: &str, count: u32, level: u8) -> String {
    let target = humanize(signature);
    if level == 1 {
        format!(
            "You have issued the same read ({target}) {count} times in this turn \
             without making progress; re-reading returns byte-for-byte identical \
             content. Stop repeating it — act on what you already have, read a \
             *different* file or line range, or take a concrete next step toward \
             the goal."
        )
    } else {
        format!(
            "You are still repeating the same read ({target}) — now {count} times. \
             This is a loop; reading it again cannot change the result. You must \
             change approach now: use the information you already have to make \
             progress, or, if you genuinely cannot proceed, say so explicitly or \
             call `abort`."
        )
    }
}

/// Turn a machine signature (`name|path|...` or `name|<raw args>`) into a short
/// human phrase for the nudge, e.g. `read_file src/main.rs`.
fn humanize(signature: &str) -> String {
    let mut fields = signature.splitn(2, '|');
    let name = fields.next().unwrap_or("").trim();
    let rest = fields.next().unwrap_or("");
    let head = rest.split('|').next().unwrap_or(rest).trim();
    if head.is_empty() {
        name.to_string()
    } else {
        format!("{name} {head}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str, offset: u64, limit: u64) -> RoundClass {
        let args = format!(r#"{{"path":"{path}","offset":{offset},"limit":{limit}}}"#);
        RoundClass::Read(read_signature([("read_file", args.as_str())]))
    }

    #[test]
    fn identical_reads_trip_at_threshold_and_only_once() {
        let mut guard = ReadLoopGuard::new();
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 1st
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 2nd
        let nudge = guard.observe(read("a.rs", 1, 50)).expect("3rd fires");
        assert!(nudge.contains("read_file a.rs"));
        // Same signature again: latched, no repeat nudge until escalation.
        assert!(guard.observe(read("a.rs", 1, 50)).is_none());
        assert!(guard.observe(read("a.rs", 1, 50)).is_none());
    }

    #[test]
    fn oscillation_between_two_pages_is_caught() {
        // A B A B A — no signature is ever consecutive, but A reaches 3 in the
        // window. A consecutive counter would miss this; the frequency window
        // does not.
        let mut guard = ReadLoopGuard::new();
        assert!(guard.observe(read("big.rs", 1, 100)).is_none()); // A
        assert!(guard.observe(read("big.rs", 900, 100)).is_none()); // B
        assert!(guard.observe(read("big.rs", 1, 100)).is_none()); // A (2)
        assert!(guard.observe(read("big.rs", 900, 100)).is_none()); // B (2)
        let nudge = guard.observe(read("big.rs", 1, 100)).expect("A hits 3");
        assert!(nudge.contains("big.rs"));
    }

    #[test]
    fn legitimate_paging_never_fires() {
        // Forward paging reads the same file but distinct ranges every time, so
        // no signature repeats.
        let mut guard = ReadLoopGuard::new();
        for page in 0..6 {
            let offset = 1 + page * 100;
            assert!(
                guard.observe(read("big.rs", offset, 100)).is_none(),
                "page at offset {offset} must not nudge"
            );
        }
    }

    #[test]
    fn a_progress_round_resets_the_window() {
        let mut guard = ReadLoopGuard::new();
        guard.observe(read("a.rs", 1, 50));
        guard.observe(read("a.rs", 1, 50));
        // A write/execute round breaks the loop: the count restarts.
        assert!(guard.observe(RoundClass::Progress).is_none());
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 1st of a fresh streak
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 2nd
        assert!(guard.observe(read("a.rs", 1, 50)).is_some()); // 3rd fires again
    }

    #[test]
    fn escalates_once_then_stays_silent() {
        let mut guard = ReadLoopGuard::new();
        let mut nudges = 0;
        // Ten identical reads: one level-1 nudge at count 3, one level-2 at 6,
        // nothing after.
        for _ in 0..10 {
            if let Some(text) = guard.observe(read("a.rs", 1, 50)) {
                nudges += 1;
                if nudges == 2 {
                    assert!(text.contains("still repeating"), "2nd nudge escalates");
                }
            }
        }
        assert_eq!(nudges, 2, "fires exactly twice (confirm + escalate)");
    }

    #[test]
    fn distinct_grep_queries_on_one_file_are_not_collapsed() {
        // Same path, different patterns -> different signatures (query-shaped
        // args fall back to raw), so a real search is never mistaken for a loop.
        let mut guard = ReadLoopGuard::new();
        let sig = |pat: &str| {
            RoundClass::Read(read_signature([(
                "grep",
                Box::leak(format!(r#"{{"pattern":"{pat}","path":"a.rs"}}"#).into_boxed_str())
                    as &str,
            )]))
        };
        assert!(guard.observe(sig("foo")).is_none());
        assert!(guard.observe(sig("bar")).is_none());
        assert!(guard.observe(sig("baz")).is_none());
    }

    #[test]
    fn pagination_default_toggling_does_not_dodge_the_guard() {
        // "read a.rs", "read a.rs offset=1", "read a.rs offset=1 limit=0" all
        // mean the same read; they must share a signature.
        let mut guard = ReadLoopGuard::new();
        let bare = RoundClass::Read(read_signature([("read_file", r#"{"path":"a.rs"}"#)]));
        let with_offset = RoundClass::Read(read_signature([(
            "read_file",
            r#"{"path":"a.rs","offset":1}"#,
        )]));
        let full = read("a.rs", 1, 0);
        assert!(guard.observe(bare).is_none());
        assert!(guard.observe(with_offset).is_none());
        assert!(guard.observe(full).is_some(), "all three are the same read");
    }
}
