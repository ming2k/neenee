//! Translation between the harness's persistent [`Message`] stream and the
//! TUI's semantic [`TranscriptMessage`] document model. Also hosts the small
//! parsing helpers for the textual `[<tool> result]:` envelope and the
//! matching `Calling \`<tool>\`` formatter used when no display content is
//! available for a restored assistant turn.
//!
//! [`Message`]: neenee_core::Message

use neenee_core::{Message, Role};

use crate::tui::config::{self, TuiConfig};
use crate::tui::document::TranscriptMessage;
use crate::tui::step_interaction;

pub(super) fn transcript_message_from_core(message: Message) -> Option<TranscriptMessage> {
    if message.hidden || message.role == Role::System {
        return None;
    }
    let provider = message.provider.clone();
    let model = message.model.clone();
    let content = if let Some(display_content) = message.display_content {
        display_content
    } else if message.content.is_empty() {
        message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|call| format_tool_call(&call.name, &call.arguments))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        message.content
    };
    if content.is_empty() {
        None
    } else {
        let mut msg = TranscriptMessage::new(message.role, content);
        msg.provider = provider;
        msg.model = model;
        Some(msg)
    }
}

pub(super) fn transcript_messages_from_core(
    messages: Vec<Message>,
    config: &TuiConfig,
) -> Vec<TranscriptMessage> {
    let mut restored = Vec::new();
    for mut message in messages {
        if message.hidden || message.role == Role::System {
            continue;
        }
        // Attribution travels on every part so a resumed session that mixed
        // models still shows which model produced each turn.
        let provider = message.provider.clone();
        let model = message.model.clone();
        if message.role == Role::Assistant {
            if let Some(reasoning) = message.reasoning_content.take() {
                let mut thinking = TranscriptMessage::thinking(reasoning);
                thinking.provider = provider.clone();
                thinking.model = model.clone();
                thinking.set_thinking_duration(0);
                // Honor the configured default expand state for reasoning
                // traces so resumed sessions match live behavior.
                if config::thinking_default_expanded(config) {
                    thinking.set_thinking_expanded(true);
                }
                restored.push(thinking);
            }
            if let Some(calls) = message.tool_calls.take() {
                for call in calls {
                    // Historical results match by tool name, so use it as the id.
                    // Disclosure is applied when the matching result finishes
                    // the step below (lifecycle-aware default), mirroring live.
                    let mut step =
                        TranscriptMessage::tool_step(call.name.clone(), call.name, call.arguments);
                    step.provider = provider.clone();
                    step.model = model.clone();
                    restored.push(step);
                }
                if message.content.is_empty() {
                    continue;
                }
            }
        }
        if message.role == Role::Tool {
            if let Some((name, output)) = parse_tool_result(&message.content) {
                let mut finished = false;
                for item in restored.iter_mut() {
                    if item.finish_tool_step(name, output, neenee_core::ToolOutput::text(output), 0)
                    {
                        // Apply the lifecycle-aware default disclosure so
                        // restored steps match live (Failed/Denied expand,
                        // Ok follows per-tool config).
                        if let Some(status) = item.tool_step_status() {
                            let default = step_interaction::default_tool_expanded(
                                status, name, config, false,
                            );
                            item.set_tool_step_expanded(default);
                        }
                        finished = true;
                        break;
                    }
                }
                if finished {
                    continue;
                }
            }
        }
        if let Some(message) = transcript_message_from_core(message) {
            restored.push(message);
        }
    }
    restored
}

/// Freeze any in-flight reasoning traces in `messages`.
///
/// A reasoning trace is rendered as "running" (breathing spinner) for as
/// long as its `duration_ms` is `None`. The trace normally reaches that
/// terminal state when `StreamReasoningEnd` arrives. But when a turn ends
/// first — the user interrupts, the provider errors mid-stream, or a fresh
/// turn supersedes a still-streaming one — that event never arrives and the
/// spinner would breathe forever. This sweep stamps `duration_ms` on every
/// still-streaming trace so the marker freezes on its last token.
///
/// `duration_ms` is the elapsed reasoning time if known (e.g. captured from
/// the stream start); `None` means the start instant was already consumed
/// or never recorded, in which case `0` is used so the trace still leaves
/// the streaming state.
pub(super) fn finalize_streaming_reasoning(
    messages: &mut [TranscriptMessage],
    duration_ms: Option<u64>,
) {
    let stamped = duration_ms.unwrap_or(0);
    for message in messages.iter_mut() {
        if message.is_thinking_streaming() {
            message.set_thinking_duration(stamped);
        }
    }
}

pub(super) fn parse_tool_result(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('[')?;
    let (name, output) = content.split_once(" result]:")?;
    Some((name, output.trim_start_matches('\n')))
}

pub(super) fn format_tool_call(name: &str, arguments: &str) -> String {
    let arguments = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_string());
    format!("Calling `{}`\n\n```json\n{}\n```", name, arguments)
}
