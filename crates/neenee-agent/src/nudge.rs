//! Nudge configuration and steering-message construction.
//!
//! Extracted from `loop_guard.rs` so the read-loop detector stays a pure
//! bookkeeping device while the *thresholds* and *wording* of the steering
//! nudges live behind a single serializable config ([`NudgeConfig`]) that the
//! `/config` modal edits at runtime.
//!
//! ## Default is off
//!
//! [`NudgeConfig::default`] has `enabled = false`. The deterministic read-loop
//! guard is an opt-in safety net, not a default-on interruption: a model that
//! is making progress should never see a nudge, and a model that is genuinely
//! stuck has the `abort` tool and the user has `Esc`. Turn it on via the
//! `/config` modal or the `[nudge]` table in `config.toml` when you want the
//! harness to break read-loops automatically.
//!
//! ## Where the thresholds live
//!
//! The detector in `loop_guard.rs` keeps its sliding-window algorithm; the
//! *constants* that govern when a window count trips a nudge (`window`,
//! `threshold`, `escalate_at`, `path_threshold`) are read from
//! [`NudgeConfig`], so a user can tune sensitivity without touching code. The
//! message templates below are pure functions of the (signature, count, level)
//! triple the detector emits — they carry the anti-anchoring wording that
//! breaks the loop, and are shared by the exact-signature and path-bucket
//! axes.

use neenee_core::NudgeConfig;
use serde_json::Value;

// ── default thresholds ───────────────────────────────────────────────────
// These are the values [`NudgeConfig::default`] seeds. Kept here as named
// constants (rather than inlined in the `Default` impl) so the detector, the
// config docs, and the `/config` modal can all reference the documented
// baseline without restating the numbers.

/// Default sliding-window size: how many recent read-rounds are considered
/// when judging whether a signature is recurring. Large enough to span a
/// `A B A B` thrash, small enough that an old, since-abandoned read ages out
/// and stops counting.
pub const DEFAULT_WINDOW: usize = 8;

/// Default exact-signature occurrences within the window that constitute a
/// loop. Two could be a legitimate "read, glance away, re-read"; three in an
/// 8-round window is not plausibly productive.
pub const DEFAULT_THRESHOLD: u32 = 3;

/// Default escalation point: if a signature reaches this many occurrences
/// after the first nudge was already sent, escalate from a soft
/// [`GuardAction::Inject`](crate::loop_guard::GuardAction::Inject) to a hard
/// [`GuardAction::Block`](crate::loop_guard::GuardAction::Block) — the read
/// signature is masked for the rest of the turn so it physically cannot
/// recur. Beyond this we stay silent and let the hard backstops
/// (`hard_stop_rounds`, `abort`, `Esc`) take over.
pub const DEFAULT_ESCALATE_AT: u32 = 6;

/// Default path-bucket threshold: occurrences of the same *path bucket*
/// (`name|path`) within the window that constitute a similar-parameter loop.
/// Higher than [`DEFAULT_THRESHOLD`] (which targets exact duplicates):
/// re-reading the same file at a few different offsets is often legitimate
/// exploration, so this axis is deliberately more permissive. Set at
/// [`DEFAULT_WINDOW`] (8) rather than 5 so that genuine forward-paging of a
/// large file — reading offset 1, 101, 201, … — is not mistaken for a loop.
pub const DEFAULT_PATH_THRESHOLD: u32 = 8;

// ── message construction ─────────────────────────────────────────────────
// The wording is a fixed template — the *information* (you repeated this, it
// is unchanged, change course) is what breaks the anchor, not eloquence, so
// it needs no model call to compose. Kept here so the detector stays
// wordless and the templates are editable in one place.

/// Build the level-1 nudge text — the first, informative break for the
/// exact-signature axis. Called only for level 1: level-2 escalation produces
/// a [`crate::loop_guard::GuardAction::Block`] with [`build_block_nudge`]
/// instead.
pub fn build_nudge(signature: &str, count: u32, level: u8) -> String {
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

/// Nudge for the path-bucket axis: the model is re-reading the same file at
/// different offsets. The message differs from the exact-signature nudge
/// because each read *did* return different content — the problem is the
/// model is stuck on one file instead of acting on what it has.
pub fn build_path_nudge(signature: &str, count: u32, level: u8) -> String {
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

/// Message paired with a
/// [`crate::loop_guard::GuardAction::Block`] on the exact-signature axis.
/// Distinct from [`build_nudge`]: this one *announces* the hard block the
/// engine is now enforcing, so the model understands why its read will be
/// refused and what to do instead. It is injected as a hidden user message
/// at the same time the signature is masked, so the block and its rationale
/// arrive together.
pub fn build_block_nudge(target: &str, count: u32) -> String {
    format!(
        "You have repeated the same read ({target}) {count} times this turn \
         despite a prior warning. This exact read is now **blocked** for the \
         rest of the turn — calling it again will return an error, not the \
         content. You already have this content in context. Act on it now: use \
         `edit_file`/`write_file` to make the change you keep re-reading for, \
         or read a *different* file or line range. If you genuinely cannot \
         proceed, say so explicitly or call `abort`."
    )
}

/// Message paired with a
/// [`crate::loop_guard::GuardAction::Block`] on the path-bucket axis: the
/// model re-read the same file at too many different offsets after a
/// warning, so the whole file is now read-blocked for the turn.
pub fn build_path_block_nudge(target: &str, count: u32) -> String {
    format!(
        "You have read {target} {count} times this turn at different offsets \
         despite a prior warning, and are not making progress. This file is now \
         **read-blocked** for the rest of the turn — reading it again (at any \
         offset) will return an error. You already have enough from this file \
         in context. Act on it now: make the change you keep reading for, or \
         move to a different file or a concrete next step toward the goal. If \
         you genuinely cannot proceed, say so explicitly or call `abort`."
    )
}

// ── signature helpers ────────────────────────────────────────────────────
// Shared between the detector (`loop_guard.rs`) and the message builders
// above (which need to humanize a signature for the nudge text). Kept here so
// both axes (exact + path) read from the same canonicalization.

/// Canonical signature of an all-read round: the per-call signatures, sorted
/// and joined so a round's identity is independent of the order the model
/// happened to emit its parallel reads in.
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
/// with no path, the signature is `name` only (so distinct tools stay
/// distinct).
pub fn path_signature<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut parts: Vec<String> = calls
        .into_iter()
        .map(|(name, args)| path_call_signature(name, args))
        .collect();
    parts.sort();
    parts.join(" + ")
}

/// Path-only signature for one read call: `name|path`, ignoring pagination.
/// For query-shaped reads or path-less calls, just the tool `name`.
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
/// `limit`→0) so the model cannot dodge the guard by toggling a default, and
/// so a read of a *different* line range is correctly a *different*
/// signature (genuine paging never trips). For a query-shaped read (e.g.
/// `grep`) we fall back to the raw arguments so distinct queries stay
/// distinct.
pub fn call_signature(name: &str, args: &str) -> String {
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

/// Turn a machine signature (`name|path|...` or `name|<raw args>`) into a
/// short human phrase for the nudge, e.g. `read_text src/main.rs`.
pub fn humanize(signature: &str) -> String {
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

/// A disabled [`NudgeConfig`] convenience for sites (envoys, the review
/// diagnostic) that must run unobstructed regardless of user settings. This
/// is just [`NudgeConfig::disabled`] re-exported for ergonomic call-site
/// spelling; the canonical constructor lives on the config struct.
pub fn disabled_config() -> NudgeConfig {
    NudgeConfig::disabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let cfg = NudgeConfig::default();
        assert!(!cfg.enabled, "nudge must default to disabled");
        assert_eq!(cfg.window, DEFAULT_WINDOW);
        assert_eq!(cfg.threshold, DEFAULT_THRESHOLD);
        assert_eq!(cfg.escalate_at, DEFAULT_ESCALATE_AT);
        assert_eq!(cfg.path_threshold, DEFAULT_PATH_THRESHOLD);
    }

    #[test]
    fn disabled_helper_yields_disabled_default_thresholds() {
        // `disabled()` is the canonical "off" state: default thresholds,
        // firing off. Envoy / review paths use it to run unobstructed
        // regardless of user settings.
        let off = NudgeConfig::disabled();
        assert!(!off.enabled);
        assert_eq!(off.window, DEFAULT_WINDOW);
        assert_eq!(off.threshold, DEFAULT_THRESHOLD);
        assert_eq!(off.escalate_at, DEFAULT_ESCALATE_AT);
        assert_eq!(off.path_threshold, DEFAULT_PATH_THRESHOLD);
    }

    #[test]
    fn store_and_agent_default_thresholds_agree() {
        // `neenee-store` does not depend on `neenee-agent`, so the defaults
        // are re-stated in `NudgeConfig::default`. This test is the sync
        // tripwire: if either side drifts, this fails.
        let store_default = NudgeConfig::default();
        assert_eq!(store_default.window, DEFAULT_WINDOW);
        assert_eq!(store_default.threshold, DEFAULT_THRESHOLD);
        assert_eq!(store_default.escalate_at, DEFAULT_ESCALATE_AT);
        assert_eq!(store_default.path_threshold, DEFAULT_PATH_THRESHOLD);
    }

    #[test]
    fn build_nudge_names_the_repeated_read() {
        let msg = build_nudge("read_text|a.rs|1|50", 3, 1);
        assert!(msg.contains("read_text a.rs"), "humanized target: {msg}");
        assert!(msg.contains("3 times"), "count surfaced: {msg}");
    }

    #[test]
    fn build_path_nudge_mentions_same_file() {
        let msg = build_path_nudge("read_text|a.rs", 8, 1);
        assert!(msg.contains("a.rs"));
        assert!(msg.contains("same file"), "path axis wording: {msg}");
    }

    #[test]
    fn build_block_nudge_announces_the_block() {
        let msg = build_block_nudge("read_text a.rs", 6);
        assert!(msg.contains("blocked"));
        assert!(msg.contains("a.rs"));
    }

    #[test]
    fn read_signature_normalises_pagination_defaults() {
        // "read a.rs", "read a.rs offset=1", "read a.rs offset=1 limit=0"
        // all mean the same read; they must share a signature so the
        // detector cannot be dodged by toggling a default.
        let bare = read_signature([("read_text", r#"{"path":"a.rs"}"#)]);
        let with_offset = read_signature([("read_text", r#"{"path":"a.rs","offset":1}"#)]);
        let full = read_signature([("read_text", r#"{"path":"a.rs","offset":1,"limit":0}"#)]);
        assert_eq!(bare, with_offset);
        assert_eq!(bare, full);
    }

    #[test]
    fn path_signature_collapses_offsets_of_one_file() {
        // Re-reading a.rs at three different offsets collapses to one path
        // bucket — the similar-parameter escape the path axis exists to
        // catch.
        let a = read_signature([("read_text", r#"{"path":"a.rs","offset":1,"limit":50}"#)]);
        let b = read_signature([("read_text", r#"{"path":"a.rs","offset":50,"limit":50}"#)]);
        assert_ne!(a, b, "exact signatures differ across offsets");
        let pa = path_signature([("read_text", r#"{"path":"a.rs","offset":1,"limit":50}"#)]);
        let pb = path_signature([("read_text", r#"{"path":"a.rs","offset":50,"limit":50}"#)]);
        assert_eq!(pa, pb, "path bucket collapses offsets");
    }

    #[test]
    fn humanize_handles_path_less_signature() {
        // A query-shaped read (e.g. grep) has no path; humanize should fall
        // back to the tool name only.
        assert_eq!(humanize("grep"), "grep");
        assert_eq!(humanize("read_text|a.rs|1|50"), "read_text a.rs");
    }
}
