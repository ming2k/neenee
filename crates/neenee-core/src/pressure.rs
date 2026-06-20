//! Context-pressure accounting and relief.
//!
//! Cheap character/token estimates over message lists (used when a provider
//! does not report real usage) plus the policy that clears old `Tool`-role
//! results to relieve pressure while keeping the OpenAI `tool_call_id` chain
//! intact.

use crate::{Message, Role};

/// Placeholder written into a tool-result message whose content has been pruned
/// to relieve context pressure. Kept on a `Tool`-role message so the OpenAI
/// `tool_call_id` chain stays intact for providers that require it.
pub const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool result content cleared]";

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
    (estimate_chars(messages) / 4).max(1)
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
}
