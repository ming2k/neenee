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

// ===========================================================================
// Round-guard abstraction: a pluggable registry of per-round guards.
//
// `ReadLoopGuard` below is one implementation. A guard observes each completed
// tool round and may return a `GuardAction` — `Inject` a steering nudge, or
// `Abort` the turn. The `GuardRegistry` dispatches to all registered guards and
// merges their actions (Abort > Inject > Continue). This replaces the bespoke
// `loop_guard_enabled`/`pending_round`/`maybe_inject_loop_nudge` triplet with a
// uniform seam, so future guards (e.g. a write-rewrite guard) slot in without
// touching the loop body.
// ===========================================================================

/// The outcome a [`RoundGuard`] returns after observing a round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum GuardAction {
    /// No action — the round is fine, keep going.
    #[default]
    Continue,
    /// Inject `message` as a hidden user message before the next model request.
    /// Non-terminating: the turn keeps running.
    Inject(String),
    /// Abort the turn with `reason` as a terminal error. Hard-terminating.
    Abort(String),
}

impl GuardAction {
    /// Severity rank for merging: Abort > Inject > Continue.
    fn severity(&self) -> u8 {
        match self {
            GuardAction::Continue => 0,
            GuardAction::Inject(_) => 1,
            GuardAction::Abort(_) => 2,
        }
    }

    /// Merge two actions, keeping the more severe. For two `Inject`s of equal
    /// severity, concatenate so both nudges reach the model.
    pub fn merge(self, other: GuardAction) -> GuardAction {
        match (self.severity(), other.severity()) {
            (a, b) if a >= b => match (self, other) {
                (GuardAction::Inject(mut s), GuardAction::Inject(t)) => {
                    s.push_str("\n\n");
                    s.push_str(&t);
                    GuardAction::Inject(s)
                }
                (winner, _) => winner,
            },
            _ => other,
        }
    }
}

/// Read-only view of one completed tool round, fed to each [`RoundGuard`].
/// Borrows from the agent's `ToolCall` list so no allocation is needed.
#[derive(Debug, Clone, Copy)]
pub struct GuardRound<'a> {
    /// The tool calls issued this round: `(name, arguments)`.
    pub calls: &'a [(&'a str, &'a str)],
    /// Whether *every* call in `calls` targets an `Unspecified` scope (i.e.
    /// pure read/search, no Path/Command). The caller (which owns the
    /// tool-access classifier) computes this once and passes it in.
    pub all_read: bool,
}

/// A deterministic round-level guard. Observes each completed tool round and may
/// return a [`GuardAction`] — inject a steering nudge, or abort the turn.
///
/// A guard is stateful and scoped to one turn (lives in `TurnState`): it
/// observes each round, carrying state across rounds within that turn. Dropped
/// when the turn ends, so state never crosses turns. Implementations must be
/// cheap (no model calls) — they run on the hot round-boundary path. and is dropped
/// when the turn ends, so state never crosses turns. Implementations must be
/// cheap (no model calls) — they run on the hot round-boundary path.
pub trait RoundGuard: Send + Sync {
    /// Observe one completed round. Return an action to inject or abort.
    fn observe(&mut self, round: GuardRound<'_>) -> GuardAction;

    /// Reset all internal state (called when a progress round clears the slate,
    /// and at turn start). Default does nothing.
    fn reset(&mut self) {}
}

/// An ordered collection of [`RoundGuard`]s. Dispatches each round to all
/// registered guards and merges their actions. Lives in `TurnState`; per-turn.
#[derive(Default)]
pub struct GuardRegistry {
    guards: Vec<Box<dyn RoundGuard>>,
}

impl GuardRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a guard. Guards fire in registration order.
    pub fn register(&mut self, guard: Box<dyn RoundGuard>) {
        self.guards.push(guard);
    }

    /// Whether any guards are registered.
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }

    /// Dispatch `round` to every guard, merging their actions. The merged
    /// action is applied by the caller (inject nudge, abort turn).
    pub fn observe(&mut self, round: GuardRound<'_>) -> GuardAction {
        self.guards
            .iter_mut()
            .map(|g| g.observe(round))
            .fold(GuardAction::Continue, GuardAction::merge)
    }

    /// Reset every guard's internal state (e.g. on a progress round).
    pub fn reset(&mut self) {
        for g in &mut self.guards {
            g.reset();
        }
    }
}

/// Per-turn state bundling a [`GuardRegistry`] with the tool-call data for the
/// round just dispatched. The agent loop sets [`pending_calls`](Self::pending_calls)
/// and [`pending_all_read`](Self::pending_all_read) in `dispatch_tool_calls`,
/// then consumes them once at the round boundary via [`Self::take_action`].
///
/// This replaces the old `pending_round: RoundClass` + `loop_guard: ReadLoopGuard`
/// pair with a single uniform seam that dispatches to any number of guards.
#[derive(Default)]
pub struct RoundGuardState {
    /// The registry of guards. Dispatches to all of them.
    registry: GuardRegistry,
    /// The tool calls from the round just dispatched, stored as owned `(name,
    /// args)` pairs so they outlive the borrowed `GuardRound`. Consumed once.
    pending_calls: Vec<(String, String)>,
    /// Whether `pending_calls` was classified as all-read. `true` by default so
    /// an empty round (no calls) doesn't spuriously reset guards.
    pending_all_read: bool,
}

impl RoundGuardState {
    /// Build guard state with a pre-populated registry.
    pub fn new(registry: GuardRegistry) -> Self {
        Self {
            registry,
            pending_calls: Vec::new(),
            pending_all_read: true,
        }
    }

    /// Record the classification of the round just dispatched. Called once per
    /// round in `dispatch_tool_calls`. When `all_read` is false, also resets the
    /// guards immediately (a progress round clears the slate).
    pub fn set_round(&mut self, calls: Vec<(String, String)>, all_read: bool) {
        self.pending_calls = calls;
        self.pending_all_read = all_read;
        if !all_read {
            self.registry.reset();
        }
    }

    /// Consume the pending round: dispatch it to every guard and return the
    /// merged [`GuardAction`]. Called once at the round boundary. After this,
    /// the pending state is cleared until the next `set_round`.
    pub fn take_action(&mut self) -> GuardAction {
        if !self.pending_all_read {
            self.pending_calls.clear();
            return GuardAction::Continue;
        }
        // Build borrowed views from the owned pairs.
        let borrows: Vec<(&str, &str)> = self
            .pending_calls
            .iter()
            .map(|(n, a)| (n.as_str(), a.as_str()))
            .collect();
        let round = GuardRound {
            calls: &borrows,
            all_read: true,
        };
        let action = self.registry.observe(round);
        self.pending_calls.clear();
        action
    }
}

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

/// Occurrences of the same *path bucket* (`name|path`) within the window that
/// constitute a similar-parameter loop. Higher than [`THRESHOLD`] (which targets
/// exact duplicates): re-reading the same file at a few different offsets is
/// sometimes legitimate exploration, but 5+ reads of one file without touching
/// any other is not plausibly productive.
pub const PATH_THRESHOLD: u32 = 5;

/// How a completed tool round looks to a guard. Computed by the caller (which
/// owns tool-access classification) and fed to [`ReadLoopGuard::observe_round`].
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
///
/// Tracks two axes of repetition:
/// - **Exact signature** (`name|path|offset|limit`): catches `A A A` and the
///   two-page thrash `A B A B A`. Genuine forward paging (`A B C D E`) never
///   trips because each page is a distinct signature.
/// - **Path bucket** (`name|path`): catches the similar-parameter escape where
///   a model reads the *same file* at many different offsets (`1`, `50`, `100`,
///   `150`, …) without ever leaving that file. Each exact signature differs, so
///   the exact-signature axis misses it; the path-bucket axis catches it.
#[derive(Default)]
pub struct ReadLoopGuard {
    /// Exact signatures of the last [`WINDOW`] read rounds, oldest at the front.
    window: VecDeque<String>,
    /// Per-signature latch: how many nudges this signature has already drawn in
    /// its current streak. Cleared for a signature once it ages out of the
    /// window (its streak is over), so a later recurrence can nudge again.
    nudges_sent: HashMap<String, u8>,
    /// Path-bucket signatures (`name|path`) of the last [`WINDOW`] read rounds.
    /// Catches the similar-parameter escape: re-reading the same file at
    /// different offsets. A distinct file each time never trips.
    path_window: VecDeque<String>,
    /// Per-path-bucket latch, same semantics as [`nudges_sent`](Self::nudges_sent).
    path_nudges_sent: HashMap<String, u8>,
}

impl ReadLoopGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one completed tool round and return a nudge prompt iff this round
    /// pushed a read signature to (or past) the loop threshold and the latch
    /// admits firing. `None` the rest of the time — including every non-read
    /// round, which also resets the window.
    ///
    /// This is the legacy entry point (kept for tests). The [`RoundGuard`]
    /// implementation routes through [`Self::observe_round`] which also checks
    /// the path-bucket axis.
    pub fn observe(&mut self, round: RoundClass) -> Option<String> {
        match round {
            RoundClass::Progress => {
                self.reset();
                None
            }
            RoundClass::Read(signature) => self.observe_exact(signature),
        }
    }

    /// Observe a round via the trait's [`GuardRound`] context. This is the
    /// primary entry point from the agent loop — it computes both the exact
    /// signature and the path bucket, then checks both axes.
    fn observe_round(&mut self, round: GuardRound<'_>) -> GuardAction {
        if !round.all_read {
            self.reset();
            return GuardAction::Continue;
        }
        let exact = read_signature(round.calls.iter().copied());
        let path = path_signature(round.calls.iter().copied());

        // Check both axes; the path-bucket axis catches the similar-parameter
        // escape, the exact-signature axis catches identical/thrash loops.
        if let Some(nudge) = self.observe_exact(exact.clone()) {
            return GuardAction::Inject(nudge);
        }
        // Only check path axis if the exact axis didn't fire (avoid double-fire
        // for identical reads, which the exact axis already catches with a
        // better-targeted message).
        if exact != path {
            if let Some(nudge) = self.observe_path(path) {
                return GuardAction::Inject(nudge);
            }
        }
        GuardAction::Continue
    }

    /// Exact-signature axis: observe a canonical signature, return a nudge if it
    /// reaches threshold. Non-read rounds are the caller's responsibility (call
    /// `reset` first).
    fn observe_exact(&mut self, signature: String) -> Option<String> {
        self.push(signature.clone());
        let count = self.count(&signature);
        if count < THRESHOLD {
            return None;
        }

        let already_sent = self.nudges_sent.get(&signature).copied().unwrap_or(0);
        let level = match already_sent {
            0 => 1,
            1 if count >= ESCALATE_AT => 2,
            _ => return None,
        };
        self.nudges_sent.insert(signature.clone(), level);
        Some(build_nudge(&signature, count, level))
    }

    /// Path-bucket axis: observe a path signature, return a nudge if the same
    /// file is being read at many different offsets.
    fn observe_path(&mut self, path_sig: String) -> Option<String> {
        self.push_path(path_sig.clone());
        let count = self.count_path(&path_sig);
        if count < PATH_THRESHOLD {
            return None;
        }

        let already_sent = self.path_nudges_sent.get(&path_sig).copied().unwrap_or(0);
        let level = match already_sent {
            0 => 1,
            1 if count >= ESCALATE_AT => 2,
            _ => return None,
        };
        self.path_nudges_sent.insert(path_sig.clone(), level);
        Some(build_path_nudge(&path_sig, count, level))
    }

    fn push(&mut self, signature: String) {
        self.window.push_back(signature);
        while self.window.len() > WINDOW {
            #[allow(clippy::expect_used)]
            // only popped while len > WINDOW, so non-empty by construction
            let evicted = self.window.pop_front().expect("non-empty");
            if !self.window.contains(&evicted) {
                self.nudges_sent.remove(&evicted);
            }
        }
    }

    fn push_path(&mut self, signature: String) {
        self.path_window.push_back(signature);
        while self.path_window.len() > WINDOW {
            #[allow(clippy::expect_used)]
            // only popped while len > WINDOW, so non-empty by construction
            let evicted = self.path_window.pop_front().expect("non-empty");
            if !self.path_window.contains(&evicted) {
                self.path_nudges_sent.remove(&evicted);
            }
        }
    }

    fn count(&self, signature: &str) -> u32 {
        self.window.iter().filter(|s| *s == signature).count() as u32
    }

    fn count_path(&self, signature: &str) -> u32 {
        self.path_window.iter().filter(|s| *s == signature).count() as u32
    }

    fn reset(&mut self) {
        self.window.clear();
        self.nudges_sent.clear();
        self.path_window.clear();
        self.path_nudges_sent.clear();
    }
}

impl RoundGuard for ReadLoopGuard {
    fn observe(&mut self, round: GuardRound<'_>) -> GuardAction {
        self.observe_round(round)
    }

    fn reset(&mut self) {
        ReadLoopGuard::reset(self);
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

/// Path-bucket signature of an all-read round: like [`read_signature`] but
/// collapses to `name|path` only (ignoring offset/limit), so re-reading the
/// same file at different offsets shares a signature. This is the
/// similar-parameter-escape detector. For query-shaped reads (grep) or calls
/// with no path, the signature is `name` only (so distinct tools stay distinct).
pub fn path_signature<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut parts: Vec<String> = calls
        .into_iter()
        .map(|(name, args)| path_call_signature(name, args))
        .collect();
    parts.sort();
    parts.join(" + ")
}

/// Path-only signature for one read call: `name|path`, ignoring pagination. For
/// query-shaped reads or path-less calls, just the tool `name`.
fn path_call_signature(name: &str, args: &str) -> String {
    let value: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let path = ["path", "file_path", "file", "filename"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str));
    match path {
        Some(path) if !has_query_arg(&value) => format!("{name}|{path}"),
        _ => name.to_string(),
    }
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

/// Nudge for the path-bucket axis: the model is re-reading the same file at
/// different offsets. The message differs from the exact-signature nudge because
/// each read *did* return different content — the problem is the model is
/// stuck on one file instead of acting on what it has.
fn build_path_nudge(signature: &str, count: u32, level: u8) -> String {
    let target = humanize(signature);
    if level == 1 {
        format!(
            "You have read {target} {count} times this turn, re-reading the same file \
             at different offsets without making progress. You have enough context \
             from this file — act on it, or move to a different file or a concrete next \
             step toward the goal."
        )
    } else {
        format!(
            "You are still stuck on {target} — now read {count} times. This is a loop. \
             Reading this file again cannot help: use the information you already have \
             to make progress, or, if you genuinely cannot proceed, say so explicitly \
             or call `abort`."
        )
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

    // ── path-bucket axis tests (via the RoundGuard trait) ───────────────────
    // These exercise the new path-bucket detector that catches the
    // similar-parameter escape: re-reading the same file at many different
    // offsets, where each exact signature differs so the exact-signature axis
    // never trips.

    fn guard_round<'a>(calls: &'a [(&'a str, &'a str)]) -> GuardRound<'a> {
        GuardRound {
            calls,
            all_read: true,
        }
    }

    /// Drive a `ReadLoopGuard` through the `RoundGuard` trait — the production
    /// path. This is what the agent loop does via `GuardRegistry`.
    fn trait_observe(guard: &mut ReadLoopGuard, round: GuardRound<'_>) -> GuardAction {
        RoundGuard::observe(guard, round)
    }

    #[test]
    fn path_bucket_catches_same_file_many_offsets() {
        // The bug this fixes: read a.rs at offset 1, 50, 100, 150, 200 — five
        // distinct exact signatures, so the exact axis never trips. But the
        // path-bucket axis collapses them all to "read_file|a.rs" and trips at
        // PATH_THRESHOLD=5.
        let mut guard = ReadLoopGuard::new();
        for offset in [1, 50, 100, 150, 200] {
            let args = format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
            let call = ("read_file", args.as_str());
            let action = trait_observe(&mut guard, guard_round(&[call]));
            if offset < 200 {
                assert_eq!(
                    action,
                    GuardAction::Continue,
                    "offset {offset} should not trip"
                );
            } else {
                match action {
                    GuardAction::Inject(msg) => assert!(
                        msg.contains("a.rs") && msg.contains("same file"),
                        "path nudge should name the file and mention the issue, got: {msg}"
                    ),
                    other => panic!("offset 200 should trip path axis, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn path_bucket_does_not_fire_on_distinct_files() {
        // Reading 5 different files is legitimate exploration — the path axis
        // must not trip.
        let mut guard = ReadLoopGuard::new();
        for file in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"] {
            let args = format!(r#"{{"path":"{file}","offset":1,"limit":50}}"#);
            let call = ("read_file", args.as_str());
            assert_eq!(
                trait_observe(&mut guard, guard_round(&[call])),
                GuardAction::Continue,
                "reading {file} should not trip"
            );
        }
    }

    #[test]
    fn progress_round_resets_both_axes() {
        // A non-read round (all_read=false) clears both the exact-signature and
        // path-bucket windows.
        let mut guard = ReadLoopGuard::new();
        for offset in [1, 50, 100] {
            let args = format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
            let call = ("read_file", args.as_str());
            trait_observe(&mut guard, guard_round(&[call]));
        }
        // Progress round: a write.
        let progress = GuardRound {
            calls: &[],
            all_read: false,
        };
        trait_observe(&mut guard, progress);
        // After reset, the same file can be read a few times without tripping.
        for offset in [1, 50] {
            let args = format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
            let call = ("read_file", args.as_str());
            assert_eq!(
                trait_observe(&mut guard, guard_round(&[call])),
                GuardAction::Continue,
                "after progress reset, reads should not trip"
            );
        }
    }

    #[test]
    fn guard_registry_merges_actions() {
        // Two guards that both fire: the registry merges their Inject actions.
        struct AlwaysNudge {
            label: &'static str,
            fired: bool,
        }
        impl RoundGuard for AlwaysNudge {
            fn observe(&mut self, _round: GuardRound<'_>) -> GuardAction {
                if self.fired {
                    GuardAction::Continue
                } else {
                    self.fired = true;
                    GuardAction::Inject(format!("nudge-{}", self.label))
                }
            }
        }
        let mut registry = GuardRegistry::new();
        registry.register(Box::new(AlwaysNudge {
            label: "a",
            fired: false,
        }));
        registry.register(Box::new(AlwaysNudge {
            label: "b",
            fired: false,
        }));
        let action = registry.observe(GuardRound {
            calls: &[],
            all_read: true,
        });
        match action {
            GuardAction::Inject(msg) => {
                assert!(msg.contains("nudge-a"), "merged nudge missing a: {msg}");
                assert!(msg.contains("nudge-b"), "merged nudge missing b: {msg}");
            }
            other => panic!("expected Inject, got {other:?}"),
        }
    }
}
