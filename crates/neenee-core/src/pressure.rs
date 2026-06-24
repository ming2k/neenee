//! Context-pressure accounting and relief.
//!
//! Cheap character/token estimates over message lists (used when a provider
//! does not report real usage) plus the policy that clears old `Tool`-role
//! results to relieve pressure while keeping the OpenAI `tool_call_id` chain
//! intact. Compaction thresholds are derived from the active model's context
//! window via [`CompactionPolicy`] / [`ContextBudget`].

use crate::{Message, Role};
use serde::{Deserialize, Serialize};

/// Approximate characters per token for the cheap estimator used when a
/// provider does not report real token usage. Centralised here so every
/// budget↔content conversion stays consistent with [`estimate_tokens`].
pub const CHARS_PER_TOKEN: usize = 4;

/// Placeholder written into a tool-result message whose content has been pruned
/// to relieve context pressure. Kept on a `Tool`-role message so the OpenAI
/// `tool_call_id` chain stays intact for providers that require it.
pub const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool result content cleared]";

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

/// Clear the content of older `Tool`-role messages to relieve context pressure,
/// protecting the most recent `protect_recent_chars` of tool results. Mutates
/// `messages` in place. Returns `Some(PruneOutcome)` only when at least
/// `min_reclaim_chars` would be reclaimed; otherwise returns `None` and leaves
/// the messages untouched. Idempotent: already-pruned tool results are skipped.
pub fn prune_tool_results(
    messages: &mut [Message],
    protect_recent_chars: usize,
    min_reclaim_chars: usize,
) -> Option<PruneOutcome> {
    let tools: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::Tool && message.content != PRUNED_TOOL_PLACEHOLDER
        })
        .map(|(index, message)| (index, message_chars(message)))
        .collect();
    if tools.is_empty() {
        return None;
    }

    // Walk the most recent tool results backward, protecting them until the
    // protected budget is met. Older tool results become pruning candidates.
    let mut protected_chars = 0usize;
    let mut protected_count = 0usize;
    for &(_, chars) in tools.iter().rev() {
        if protected_chars >= protect_recent_chars {
            break;
        }
        protected_chars += chars;
        protected_count += 1;
    }
    let prunable_count = tools.len().saturating_sub(protected_count);
    if prunable_count == 0 {
        return None;
    }

    let reclaimable: usize = tools
        .iter()
        .take(prunable_count)
        .map(|(_, chars)| chars.saturating_sub(PRUNED_TOOL_PLACEHOLDER.len()))
        .sum();
    if reclaimable < min_reclaim_chars {
        return None;
    }

    let mut outcome = PruneOutcome::default();
    for &(index, _) in tools.iter().take(prunable_count) {
        let original = messages[index].clone();
        outcome.reclaimed_chars +=
            message_chars(&messages[index]).saturating_sub(PRUNED_TOOL_PLACEHOLDER.len());
        outcome.cleared_count += 1;
        outcome.originals.push(original);
        messages[index].content = PRUNED_TOOL_PLACEHOLDER.to_string();
        messages[index].reasoning_content = None;
        // Recursively clear old tool results inside any nested sub-agent
        // transcript. The sub-agent's `Tool`-role children hold the same kind
        // of bulky old outputs the top-level pruner is trying to reclaim;
        // leaving them intact would defeat pruning for any session that made
        // heavy use of `task`. The parent `Tool` message itself stays (the
        // OpenAI tool_call_id chain must remain intact) — only its nested
        // grandchildren are pruned, mirroring the top-level policy.
        if let Some(children) = messages[index].children.as_mut() {
            prune_tool_results_inner(children, protect_recent_chars);
        }
    }
    Some(outcome)
}

/// Inner recursive worker for [`prune_tool_results`]. Used to descend into a
/// sub-agent's nested transcript and prune its old `Tool`-role messages using
/// the same `protect_recent_chars` budget. Unlike the public entry point this
/// always prunes every eligible old result (no `min_reclaim_chars` gate) and
/// returns nothing — the durable archival already happened when the sub-agent
/// finished; here we are only relieving in-memory context pressures on resume.
fn prune_tool_results_inner(messages: &mut [Message], protect_recent_chars: usize) {
    let tools: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::Tool && message.content != PRUNED_TOOL_PLACEHOLDER
        })
        .map(|(index, _)| index)
        .collect();
    if tools.is_empty() {
        return;
    }
    let mut protected_chars = 0usize;
    let mut protected_count = 0usize;
    for &index in tools.iter().rev() {
        if protected_chars >= protect_recent_chars {
            break;
        }
        protected_chars += message_chars(&messages[index]);
        protected_count += 1;
    }
    let prunable_count = tools.len().saturating_sub(protected_count);
    for &index in tools.iter().take(prunable_count) {
        messages[index].content = PRUNED_TOOL_PLACEHOLDER.to_string();
        messages[index].reasoning_content = None;
        // Recurse one more level for sub-sub-agents (bounded by the schema's
        // tool-filter rule that prevents `task` from spawning `task`).
        if let Some(children) = messages[index].children.as_mut() {
            prune_tool_results_inner(children, protect_recent_chars);
        }
    }
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

    #[test]
    fn prune_protects_recent_tool_results_and_skips_already_pruned() {
        let big = "Y".repeat(2_000);
        let mut messages = vec![
            Message::new(Role::User, "q1"),
            Message::tool_result(
                &ToolCall {
                    id: "c1".to_string(),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                },
                big.clone(),
            ),
            Message::tool_result(
                &ToolCall {
                    id: "c2".to_string(),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                },
                big.clone(),
            ),
            Message::new(Role::User, "q2"),
        ];

        // Protect nothing (0), require at least 1 char reclaimed: the two old
        // tool results are both prunable.
        let outcome = prune_tool_results(&mut messages, 0, 1).unwrap();
        assert_eq!(outcome.cleared_count, 2);
        assert_eq!(outcome.originals.len(), 2);
        assert_eq!(messages[1].content, PRUNED_TOOL_PLACEHOLDER);
        assert_eq!(messages[2].content, PRUNED_TOOL_PLACEHOLDER);

        // Idempotent: a second pass finds nothing to prune (placeholders skipped).
        assert!(prune_tool_results(&mut messages, 0, 1).is_none());
    }

    #[test]
    fn prune_respects_protect_budget_and_min_reclaim() {
        let big = "Z".repeat(2_000);
        let mut messages = vec![Message::tool_result(
            &ToolCall {
                id: "c".to_string(),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            },
            big,
        )];

        // The single tool result is fully protected by a large budget.
        assert!(prune_tool_results(&mut messages, 10_000, 1).is_none());
        // With no protection but a reclaim minimum larger than what's available,
        // it still returns None and leaves content intact.
        assert!(prune_tool_results(&mut messages, 0, 10_000).is_none());
        assert_ne!(messages[0].content, PRUNED_TOOL_PLACEHOLDER);
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
