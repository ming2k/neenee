//! Context-pressure accounting and relief.
//!
//! Cheap character/token estimates over message lists (used when a provider
//! does not report real usage) plus the policy that clears old `Tool`-role
//! results to relieve pressure while keeping the OpenAI `tool_call_id` chain
//! intact. Compaction thresholds are derived from the active model's context
//! window via [`CompactionPolicy`] / [`ContextBudget`].

use crate::{Message, Role};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Approximate characters per token for the cheap estimator used when a
/// provider does not report real token usage. Centralised here so every
/// budget↔content conversion stays consistent with [`estimate_tokens`].
pub const CHARS_PER_TOKEN: usize = 4;

/// Legacy placeholder for a fully-cleared tool result. Still recognised on read
/// so older sessions (and the early uninformative form) are treated as
/// already-cleared, but new clears use the informative [`CLEARED_TOOL_PREFIX`]
/// form below. Kept on a `Tool`-role message so the OpenAI `tool_call_id` chain
/// stays intact for providers that require it.
pub const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool result content cleared]";

/// Prefix of the placeholder a *fully cleared* tool result is replaced with.
/// The full form carries a breadcrumb — `[cleared tool result: read foo.rs (42
/// lines, 1500 chars)]` — so the model can decide whether to re-fetch instead of
/// guessing, and so later passes recognise an already-cleared result.
pub const CLEARED_TOOL_PREFIX: &str = "[cleared tool result:";

/// Marker embedded in a *truncated* tool result (head + tail kept, middle
/// elided). Distinguishes the intermediate "truncated" tier from a full clear so
/// a later, higher-pressure pass can escalate truncation to a clear.
const ELIDED_MARKER: &str = " chars elided to relieve context ...]";

/// Own-content length (chars) above which a prune candidate is first *truncated*
/// (a gentler tier that keeps the shape of the output) rather than cleared
/// outright. Below it, truncation would not save enough to be worth the lost
/// signal, so the candidate is cleared directly.
const TRUNCATE_MIN_CHARS: usize = 2_000;

/// Characters of head and of tail preserved when truncating a candidate.
const TRUNCATE_KEEP_EACH_SIDE: usize = 400;

/// Declarative context-compaction policy expressed as fractions of the active
/// model's context window, plus a fallback window for models whose size the
/// registry does not know. Pure data; resolved into absolute token thresholds
/// for one concrete model by [`CompactionPolicy::resolve`].
///
/// Pressure is measured in tokens via [`estimate_tokens`] (or, in future, real
/// `prompt_tokens` reported by the provider), so these thresholds compare
/// directly against a model's token-denominated context window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionPolicy {
    /// Trigger a full summarizing compaction once pressure reaches this fraction
    /// of the window. The remaining headroom absorbs finishing the current turn
    /// and the summarization call itself, so a value near `1.0` risks overflow.
    pub utilization: f64,
    /// After a full compaction, compress the active window down to this fraction
    /// of the window. Lower values compact less often but deeper — the right
    /// tradeoff for an agentic loop that may run hundreds of rounds.
    pub target_utilization: f64,
    /// Trigger cheap tool-result pruning once pressure reaches this fraction,
    /// below `utilization`. Pruning keeps the tool-call id chain intact, so it
    /// is safe to run earlier and more often than a full compaction.
    pub prune_utilization: f64,
    /// Assumed window (tokens) when the active model's context window is unknown
    /// (the registry resolves to `0`). Conservative, so unknown / local models
    /// still relieve pressure instead of overflowing.
    pub fallback_window_tokens: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            utilization: 0.85,
            target_utilization: 0.25,
            prune_utilization: 0.65,
            fallback_window_tokens: 32_000,
        }
    }
}

impl CompactionPolicy {
    /// Resolve into absolute token thresholds for a concrete model window.
    /// `window_tokens == 0` (unknown model) substitutes the fallback window so
    /// compaction still engages.
    pub fn resolve(&self, window_tokens: usize) -> ContextBudget {
        let window = if window_tokens == 0 {
            self.fallback_window_tokens
        } else {
            window_tokens
        };
        let threshold = |fraction: f64| (window as f64 * fraction) as usize;
        ContextBudget {
            window_tokens: window,
            prune_threshold_tokens: threshold(self.prune_utilization),
            compaction_threshold_tokens: threshold(self.utilization),
            target_tokens: threshold(self.target_utilization),
        }
    }
}

/// Resolved, model-specific token thresholds — the runtime form of a
/// [`CompactionPolicy`] against one active model. Pressure measured by
/// [`estimate_tokens`] is compared against these; content-level sizing (summary
/// budgets, pruning protect budgets) is derived from them in characters.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    /// The window used to derive these thresholds (the fallback value when the
    /// model's real window is unknown).
    pub window_tokens: usize,
    /// Cheap tool-result pruning fires above this many tokens.
    pub prune_threshold_tokens: usize,
    /// A full summarizing compaction fires above this many tokens.
    pub compaction_threshold_tokens: usize,
    /// Post-compaction active-window target, in tokens.
    pub target_tokens: usize,
}

/// Character-size estimate of a message list: byte length of `content` +
/// tool-call `name`+`arguments`. A cheap context-pressure proxy used when a
/// provider does not report token usage. `reasoning_content` is **not**
/// included because it is never sent to providers and therefore does not
/// consume the context window.
pub fn estimate_chars(messages: &[Message]) -> usize {
    messages.iter().map(message_chars).sum()
}

/// Token estimate (~4 chars/token) of a message list.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    (estimate_chars(messages) / CHARS_PER_TOKEN).max(1)
}

// NOTE: the provider-reported-usage path (ADR-0019/0023 "layered token
// accounting", `effective_pressure_tokens` / `USAGE_TRUST_FLOOR`) was removed
// as dead code: the `Provider` trait never surfaces usage, so the function had
// no production caller and only advertised a capability that does not exist.
// Pressure is computed purely from `estimate_tokens`. Revive a usage-preferring
// policy here once a provider actually reports `prompt_tokens`. See the deferral
// note in docs/adr/0023-relevance-aware-tiered-pruning-and-layered-token-accounting.md.

pub(crate) fn message_chars(message: &Message) -> usize {
    let own = message.content.len()
        + message
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|c| c.name.len() + c.arguments.len())
                    .sum::<usize>()
            })
            .unwrap_or(0);
    // Recursively count nested sub-agent transcripts. A `task` tool result
    // carries the sub-agent's full conversation as `children`, and that
    // conversation is real context weight the parent model is effectively
    // paying for (it sees the summary, but the children live in the same
    // session.json and survive resume — both the context-pressure meter and
    // the compaction budget must see them).
    let nested = message
        .children
        .as_ref()
        .map(|children| children.iter().map(message_chars).sum::<usize>())
        .unwrap_or(0);
    own + nested
}

#[derive(Debug, Clone, Default)]
pub struct PruneOutcome {
    /// Number of tool-result messages whose content was cleared.
    pub cleared_count: usize,
    /// Character bytes reclaimed by clearing.
    pub reclaimed_chars: usize,
    /// Original (pre-clear) tool messages, oldest-first, for durable archival.
    pub originals: Vec<Message>,
}

/// Relieve context pressure by degrading older `Tool`-role results in place,
/// keeping the OpenAI `tool_call_id` chain intact. Mutates `messages`; returns
/// `Some(PruneOutcome)` only when at least `min_reclaim_chars` would be
/// reclaimed, else `None` with `messages` untouched (atomic: nothing is mutated
/// unless the gate passes).
///
/// This is more than FIFO-by-age. For each candidate the policy chooses *what*
/// to prune and *how hard*:
///
/// - **Recency protection** keeps the most recent `protect_recent_chars` of
///   tool output verbatim — that is what is usually still relevant.
/// - **Keep-alive** spares a fresh result whose file target is mentioned in the
///   last few non-tool messages (likely still in play).
/// - **Staleness / dedup** clears a result outright when a *later* tool touched
///   the same file (an earlier `read` superseded by a re-`read` or `edit` is
///   stale — and keeping stale content is worse than clearing it).
/// - **Tiered degradation** truncates a large, fresh result to head + tail first
///   (a gentler tier that keeps its shape) and only fully clears it on a later,
///   higher-pressure pass — or immediately when it is already small.
/// - **Informative clears** replace content with `[cleared tool result: <label>
///   (<n> lines, <m> chars)]` so the model can decide whether to re-fetch.
///
/// Idempotent: already-cleared results are skipped; a truncated result escalates
/// to a clear on a subsequent pass, so repeated calls converge.
pub fn prune_tool_results(
    messages: &mut [Message],
    protect_recent_chars: usize,
    min_reclaim_chars: usize,
) -> Option<PruneOutcome> {
    let plan = plan_prune(messages, protect_recent_chars);
    let reclaimable: usize = plan.iter().map(|c| c.reclaim).sum();
    if plan.is_empty() || reclaimable < min_reclaim_chars {
        return None;
    }
    Some(apply_prune(messages, plan, protect_recent_chars))
}

/// One planned degradation: replace `messages[index].content` with
/// `new_content`, reclaiming `reclaim` chars of own content.
struct PrunePlan {
    index: usize,
    new_content: String,
    reclaim: usize,
}

/// Owned (mutation-safe) summary of the tool call that produced a result: a
/// short human label and the file path it targeted, correlated via
/// `tool_call_id`.
#[derive(Clone, Default)]
struct ToolMeta {
    label: String,
    file_key: Option<String>,
}

/// Plan (without mutating) which tool results to degrade and how. Returns an
/// empty vec when there is nothing to do.
fn plan_prune(messages: &[Message], protect_recent_chars: usize) -> Vec<PrunePlan> {
    let meta = collect_tool_meta(messages);
    let tools: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Tool && !is_cleared(&m.content))
        .map(|(i, _)| i)
        .collect();
    if tools.is_empty() {
        return Vec::new();
    }

    // Recency protection: protect newest results until the char budget is met.
    let mut protected: HashSet<usize> = HashSet::new();
    let mut protected_chars = 0usize;
    for &i in tools.iter().rev() {
        if protected_chars >= protect_recent_chars {
            break;
        }
        protected_chars += message_chars(&messages[i]);
        protected.insert(i);
    }

    // Staleness: the last tool result for a given file is live; earlier ones
    // touching the same file are stale once it is re-touched.
    let mut last_for_file: HashMap<&str, usize> = HashMap::new();
    for &i in &tools {
        if let Some(fk) = meta.get(&i).and_then(|m| m.file_key.as_deref()) {
            last_for_file.insert(fk, i);
        }
    }
    let mut plan = Vec::new();
    for &i in &tools {
        if protected.contains(&i) {
            continue;
        }
        let meta_i = meta.get(&i).cloned().unwrap_or_default();
        let stale = meta_i
            .file_key
            .as_deref()
            .and_then(|fk| last_for_file.get(fk))
            .is_some_and(|&last| last > i);
        // Keep-alive spares a *fresh* result whose file target is still in play.
        // A stale result is cleared even if mentioned, because its content is
        // outdated. "In play" means referenced *after* this result was produced
        // — by later natural language or a later tool call on the same file.
        // Looking forward from `i` (not at a global recent window) is what stops
        // a result's own originating call from self-referencing and sparing it.
        if !stale && mentioned_after(messages, i, meta_i.file_key.as_deref()) {
            continue;
        }
        let content = &messages[i].content;
        let new_content = degrade(content, &meta_i, stale);
        if new_content.len() >= content.len() {
            continue; // no real gain
        }
        let reclaim = content.len() - new_content.len();
        plan.push(PrunePlan {
            index: i,
            new_content,
            reclaim,
        });
    }
    plan
}

/// Apply a plan, recording originals for archival and recursing into any nested
/// sub-agent transcript on the messages it touches.
fn apply_prune(
    messages: &mut [Message],
    plan: Vec<PrunePlan>,
    protect_recent_chars: usize,
) -> PruneOutcome {
    let mut outcome = PruneOutcome::default();
    for item in plan {
        outcome.originals.push(messages[item.index].clone());
        outcome.reclaimed_chars += item.reclaim;
        outcome.cleared_count += 1;
        messages[item.index].content = item.new_content;
        messages[item.index].reasoning_content = None;
        // A `task` result carries the sub-agent's whole transcript as
        // `children`; its old `Tool` results are the same kind of bulky weight,
        // so prune them too (ungated — durability already happened when the
        // sub-agent finished; here we relieve in-memory pressure).
        if let Some(children) = messages[item.index].children.as_mut() {
            let child_plan = plan_prune(children, protect_recent_chars);
            if !child_plan.is_empty() {
                let nested = apply_prune(children, child_plan, protect_recent_chars);
                outcome.reclaimed_chars += nested.reclaimed_chars;
            }
        }
    }
    outcome
}

/// Choose the degraded form of a tool result's content.
fn degrade(content: &str, meta: &ToolMeta, stale: bool) -> String {
    // Stale (superseded on the same file) or already truncated -> clear fully.
    if stale || is_truncated(content) {
        return cleared_placeholder(meta, content);
    }
    // Large and fresh -> truncate (gentler). Small -> clear directly.
    if content.len() >= TRUNCATE_MIN_CHARS {
        truncate_middle(content)
    } else {
        cleared_placeholder(meta, content)
    }
}

/// Correlate each `Tool` result with the assistant `tool_call` that produced it,
/// returning owned per-message-index metadata so later mutation is borrow-safe.
fn collect_tool_meta(messages: &[Message]) -> HashMap<usize, ToolMeta> {
    let mut by_id: HashMap<&str, (&str, &str)> = HashMap::new();
    for m in messages {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                by_id.insert(c.id.as_str(), (c.name.as_str(), c.arguments.as_str()));
            }
        }
    }
    let mut out = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role != Role::Tool {
            continue;
        }
        let meta = m
            .tool_call_id
            .as_deref()
            .and_then(|id| by_id.get(id))
            .map(|(name, args)| ToolMeta {
                label: tool_label(name, args),
                file_key: file_key(name, args),
            })
            .unwrap_or_default();
        out.insert(i, meta);
    }
    out
}

/// Whether a tool result's file target is referenced in any message *after*
/// `index` — by full path or by its bare file name, in natural-language content
/// or in the arguments of a later tool call. This is the keep-alive signal: a
/// result whose target is still being talked about or re-touched is left intact.
/// Looking forward from `index` (rather than at a global recent window) is what
/// prevents a result's own originating call from self-referencing and sparing it.
fn mentioned_after(messages: &[Message], index: usize, file_key: Option<&str>) -> bool {
    let Some(fk) = file_key else {
        return false;
    };
    if fk.is_empty() {
        return false;
    }
    let base = fk.rsplit(['/', '\\']).next().unwrap_or(fk);
    let base_match = base != fk && !base.is_empty();
    for m in messages.iter().skip(index + 1) {
        if m.content.contains(fk) || (base_match && m.content.contains(base)) {
            return true;
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                if c.arguments.contains(fk) || (base_match && c.arguments.contains(base)) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_cleared(content: &str) -> bool {
    content == PRUNED_TOOL_PLACEHOLDER || content.starts_with(CLEARED_TOOL_PREFIX)
}

fn is_truncated(content: &str) -> bool {
    content.contains(ELIDED_MARKER)
}

fn parsed_args(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null)
}

fn arg_str<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
}

/// Short label for a tool call, e.g. `read src/main.rs`, `grep "TODO"`, or just
/// `bash` when no salient argument is found.
fn tool_label(name: &str, arguments: &str) -> String {
    let args = parsed_args(arguments);
    match arg_str(
        &args,
        &[
            "path", "file_path", "file", "filename", "pattern", "query", "command", "cmd", "url",
        ],
    ) {
        Some(detail) => {
            let detail = detail.trim();
            let short: String = detail.chars().take(60).collect();
            if detail.chars().count() > 60 {
                format!("{name} {short}…")
            } else {
                format!("{name} {short}")
            }
        }
        None => name.to_string(),
    }
}

/// The file path a tool touched, used for staleness/dedup. `None` for tools that
/// are not file-addressed (e.g. `bash`, `grep` without a file).
fn file_key(_name: &str, arguments: &str) -> Option<String> {
    let args = parsed_args(arguments);
    arg_str(&args, &["path", "file_path", "file", "filename"]).map(|s| s.to_string())
}

/// Informative cleared-placeholder carrying the tool label and the size that was
/// dropped, so the model can decide whether to re-fetch.
fn cleared_placeholder(meta: &ToolMeta, content: &str) -> String {
    let label = if meta.label.is_empty() {
        "tool"
    } else {
        meta.label.as_str()
    };
    let lines = content.lines().count().max(1);
    format!(
        "{CLEARED_TOOL_PREFIX} {label} ({lines} lines, {} chars)]",
        content.len()
    )
}

/// Keep head + tail, eliding the middle with a recognisable marker. Returns the
/// content unchanged when it is too short for truncation to help.
fn truncate_middle(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let keep = TRUNCATE_KEEP_EACH_SIDE;
    if chars.len() <= keep * 2 + 64 {
        return content.to_string();
    }
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    let dropped = chars.len() - keep * 2;
    format!("{head}\n[... {dropped}{ELIDED_MARKER}\n{tail}")
}

pub fn estimate_message_tokens(message: &Message) -> i64 {
    let text_len = message.content.len();
    let tool_text: usize = message
        .tool_calls
        .as_ref()
        .map(|calls| calls.iter().map(|c| c.name.len() + c.arguments.len()).sum())
        .unwrap_or(0);
    estimate_string_tokens_len(text_len + tool_text)
}

pub fn estimate_string_tokens(s: &str) -> i64 {
    estimate_string_tokens_len(s.len())
}

fn estimate_string_tokens_len(len: usize) -> i64 {
    // Rough heuristic: ~4 characters per token for English text.
    // Providers that report real usage should override this estimate.
    (len / 4).max(1) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    /// Realistic Assistant message carrying a tool call — the way every Tool
    /// result is produced in an OpenAI-protocol transcript. The pruner correlates
    /// a result back to its call (name + arguments) via `tool_call_id`, so tests
    /// that exercise labels / staleness / keep-alive need this originating call
    /// to exist, not just the bare Tool result.
    fn assistant_with_call(id: &str, name: &str, args: &str) -> Message {
        let mut message = Message::new(Role::Assistant, "");
        message.tool_calls = Some(vec![call(id, name, args)]);
        message
    }

    #[test]
    fn large_fresh_result_is_truncated_then_cleared_on_next_pass() {
        let big = "Y".repeat(5_000);
        let mut messages = vec![
            Message::new(Role::User, "q1"),
            Message::tool_result(&call("c1", "bash", "{}"), big),
            Message::new(Role::User, "q2"),
        ];

        // Pass 1: large + fresh -> truncated (head/tail kept), not cleared.
        let out1 = prune_tool_results(&mut messages, 0, 1).unwrap();
        assert_eq!(out1.cleared_count, 1);
        assert!(is_truncated(&messages[1].content));
        assert!(!is_cleared(&messages[1].content));
        assert!(messages[1].content.len() < 5_000);

        // Pass 2: already truncated -> escalated to a full clear.
        let out2 = prune_tool_results(&mut messages, 0, 1).unwrap();
        assert_eq!(out2.cleared_count, 1);
        assert!(is_cleared(&messages[1].content));

        // Pass 3: nothing left to do -> None (converged / idempotent).
        assert!(prune_tool_results(&mut messages, 0, 1).is_none());
    }

    #[test]
    fn short_result_is_cleared_directly_with_informative_placeholder() {
        // Below TRUNCATE_MIN_CHARS, so it skips the truncate tier and is cleared
        // outright — but still larger than the placeholder, so clearing reclaims
        // real space (a result shorter than its placeholder is correctly left
        // alone: clearing it would *grow* the window).
        let body = format!("{}\n{}\n{}", "a".repeat(100), "b".repeat(100), "c".repeat(100));
        let mut messages = vec![
            assistant_with_call("c1", "read", r#"{"path":"src/config.rs"}"#),
            Message::tool_result(&call("c1", "read", r#"{"path":"src/config.rs"}"#), body),
        ];

        let out = prune_tool_results(&mut messages, 0, 1).unwrap();
        assert_eq!(out.cleared_count, 1);
        // Informative: carries tool label, line count, and char count.
        assert!(messages[1].content.starts_with(CLEARED_TOOL_PREFIX));
        assert!(messages[1].content.contains("read src/config.rs"));
        assert!(messages[1].content.contains("3 lines"));
    }

    #[test]
    fn stale_read_superseded_by_later_edit_is_cleared_first() {
        let body = "X".repeat(3_000);
        let mut messages = vec![
            // Round 1: read config.rs ...
            assistant_with_call("c1", "read", r#"{"path":"config.rs"}"#),
            Message::tool_result(&call("c1", "read", r#"{"path":"config.rs"}"#), body.clone()),
            // ... superseded by a later edit of the same file (round 2).
            assistant_with_call("c2", "edit", r#"{"path":"config.rs"}"#),
            Message::tool_result(&call("c2", "edit", r#"{"path":"config.rs"}"#), "ok".to_string()),
        ];

        let out = prune_tool_results(&mut messages, 0, 1).unwrap();
        assert_eq!(out.cleared_count, 1);
        // The stale read (index 1) is cleared outright (not merely truncated),
        // because a later op touched the same file.
        assert!(is_cleared(&messages[1].content));
        assert!(messages[1].content.contains("read config.rs"));
    }

    #[test]
    fn keep_alive_spares_a_result_mentioned_in_recent_messages() {
        let body = "Z".repeat(3_000);
        let mut messages = vec![
            assistant_with_call("c1", "read", r#"{"path":"important.rs"}"#),
            Message::tool_result(&call("c1", "read", r#"{"path":"important.rs"}"#), body),
            // A later user message references the file by name -> keep alive.
            Message::new(Role::User, "now fix the bug in important.rs please"),
        ];

        // Even with zero recency protection, the mentioned result is spared.
        assert!(prune_tool_results(&mut messages, 0, 1).is_none());
    }

    #[test]
    fn result_shorter_than_its_placeholder_is_left_alone() {
        // A result tinier than the informative placeholder that would replace it
        // yields negative reclaim — clearing it would *grow* the window. Such a
        // candidate is skipped entirely (no real gain), so it is left verbatim.
        let tiny = "ok".to_string();
        let mut messages = vec![Message::tool_result(&call("c1", "bash", "{}"), tiny)];
        assert!(prune_tool_results(&mut messages, 0, 1).is_none());
        assert_eq!(messages[0].content, "ok");
    }

    #[test]
    fn recency_protection_and_min_reclaim_gate() {
        let big = "Z".repeat(3_000);
        let mut messages = vec![Message::tool_result(&call("c", "bash", "{}"), big)];

        // Fully protected by a large recency budget -> None, untouched.
        assert!(prune_tool_results(&mut messages, 10_000, 1).is_none());
        // Reclaim floor larger than anything available -> None, untouched.
        assert!(prune_tool_results(&mut messages, 0, 1_000_000).is_none());
        assert!(!is_cleared(&messages[0].content));
        assert!(!is_truncated(&messages[0].content));
    }

    #[test]
    fn policy_resolves_thresholds_relative_to_window() {
        let policy = CompactionPolicy::default();
        let budget = policy.resolve(200_000);
        assert_eq!(budget.window_tokens, 200_000);
        assert_eq!(budget.prune_threshold_tokens, 130_000); // 65%
        assert_eq!(budget.compaction_threshold_tokens, 170_000); // 85%
        assert_eq!(budget.target_tokens, 50_000); // 25%
                                                  // Pruning trips before full compaction, and compaction leaves a target
                                                  // well below its trigger — the escalation ladder the harness relies on.
        assert!(budget.prune_threshold_tokens < budget.compaction_threshold_tokens);
        assert!(budget.target_tokens < budget.prune_threshold_tokens);
    }

    #[test]
    fn policy_falls_back_for_unknown_window() {
        let policy = CompactionPolicy::default();
        let budget = policy.resolve(0);
        assert_eq!(budget.window_tokens, 32_000);
        assert_eq!(budget.compaction_threshold_tokens, 27_200); // 85% of 32_000
    }

    #[test]
    fn policy_round_trips_through_serde_with_defaults() {
        // A config with no compaction table keeps the documented defaults.
        let policy: CompactionPolicy = toml::from_str("").unwrap();
        assert_eq!(policy, CompactionPolicy::default());

        // Explicit overrides survive a round-trip.
        let toml = r#"
            utilization = 0.9
            target_utilization = 0.2
            prune_utilization = 0.7
            fallback_window_tokens = 64_000
        "#;
        let policy: CompactionPolicy = toml::from_str(toml).unwrap();
        let budget = policy.resolve(100_000);
        assert_eq!(budget.prune_threshold_tokens, 70_000);
        assert_eq!(budget.compaction_threshold_tokens, 90_000);
    }
}
