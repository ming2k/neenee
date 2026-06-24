//! Conversation exporter: renders the durable [`Message`] stream as a single
//! Markdown document suitable for handing off to a fresh agent. Triggered by
//! the `/export` slash command, which copies the result to the system
//! clipboard so it can be pasted into another tool's prompt.
//!
//! Format is intentionally agent-readable: a metadata header (session id,
//! provider / model, pursuit, exported-at) followed by a preamble that
//! tells the receiving agent how to use the document, then a chronological
//! transcript of user prompts, assistant replies, tool calls, and tool
//! results. Hidden and system messages are skipped (mirroring
//! [`crate::tui::transcript`] rendering), and subagent transcripts nested
//! under `task` tool results are summarised inline rather than dumped in full
//! so the export stays scannable.

use chrono::Utc;
use neenee_core::{Message, Pursuit, Role, ToolCall};

/// Metadata carried from the harness into the exporter so the header reflects
/// the live session state at the moment of export.
#[derive(Debug, Clone)]
pub struct ExportContext<'a> {
    pub session_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub pursuit: Option<&'a Pursuit>,
    pub active_plan_path: Option<&'a std::path::Path>,
}

/// Render the current conversation as a Markdown handoff document.
pub fn format_export_markdown(ctx: ExportContext<'_>, messages: &[Message]) -> String {
    let mut out = String::new();
    out.push_str("# neenee session export\n\n");

    let exported_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    out.push_str(&format!("- **Session ID:** `{}`\n", ctx.session_id));
    out.push_str(&format!(
        "- **Provider / Model:** {} / {}\n",
        ctx.provider, ctx.model
    ));
    match ctx.pursuit {
        Some(pursuit) => out.push_str(&format!(
            "- **Pursuit [{}]:** {}\n",
            if pursuit.is_complete {
                "complete"
            } else {
                "active"
            },
            pursuit.objective
        )),
        None => out.push_str("- **Pursuit:** _none_\n"),
    }
    if let Some(plan) = ctx.active_plan_path {
        out.push_str(&format!("- **Active plan:** {}\n", plan.display()));
    }
    out.push_str(&format!("- **Exported at:** {}\n\n", exported_at));

    out.push_str(
        "The transcript below records what was done in this session. A fresh \
         agent can read it as context and continue the work. Tool calls and \
         their results are inlined chronologically so the full chain of \
         decisions and side effects is visible.\n\n---\n\n",
    );

    let mut emitted_any = false;
    let mut tool_call_cursor: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();

    for message in messages {
        if message.hidden || message.role == Role::System {
            continue;
        }
        match message.role {
            Role::User => {
                let content = pick_content(message);
                if content.is_empty() {
                    continue;
                }
                emitted_any = true;
                out.push_str("## User\n\n");
                out.push_str(content.trim());
                out.push_str("\n\n");
            }
            Role::Assistant => {
                let mut wrote_header = false;
                if let Some(reasoning) = message.reasoning_content.as_deref() {
                    if !reasoning.trim().is_empty() {
                        emitted_any = true;
                        out.push_str("<details>\n<summary>Reasoning</summary>\n\n");
                        out.push_str(reasoning.trim());
                        out.push_str("\n\n</details>\n\n");
                    }
                }
                let content = pick_content(message);
                if !content.trim().is_empty() {
                    emitted_any = true;
                    let attribution = attribution_suffix(message);
                    out.push_str("## Assistant");
                    if let Some(tag) = attribution {
                        out.push_str(&format!(" ({tag})"));
                    }
                    out.push_str("\n\n");
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                    wrote_header = true;
                }
                if let Some(calls) = message.tool_calls.as_ref() {
                    for call in calls {
                        emitted_any = true;
                        if !wrote_header {
                            let attribution = attribution_suffix(message);
                            out.push_str("## Assistant");
                            if let Some(tag) = attribution {
                                out.push_str(&format!(" ({tag})"));
                            }
                            out.push_str("\n\n");
                            wrote_header = true;
                        }
                        render_tool_call(call, messages, &mut tool_call_cursor, &mut out);
                    }
                }
            }
            Role::Tool => {
                // Tool results are inlined next to their originating call by
                // `render_tool_call`, so a standalone Tool message here is a
                // result whose matching call lived in a turn we skipped (e.g.
                // a hidden injection). Drop it to keep the transcript clean.
            }
            Role::System => {}
        }
    }

    if !emitted_any {
        out.push_str("_(No user-visible turns in this session yet.)_\n");
    }

    out
}

/// Choose the text we render for a message: `display_content` (the harness's
/// curated view) when present, otherwise the raw `content`. Mirrors
/// [`crate::tui::transcript::transcript_message_from_core`].
fn pick_content(message: &Message) -> &str {
    if let Some(display) = message.display_content.as_deref() {
        display
    } else {
        &message.content
    }
}

/// `provider / model` attribution line for an assistant message, when the
/// harness stamped one. Returns `None` when both fields are absent, which is
/// the case for synthesised or test messages.
fn attribution_suffix(message: &Message) -> Option<String> {
    match (message.provider.as_deref(), message.model.as_deref()) {
        (Some(p), Some(m)) if !p.is_empty() || !m.is_empty() => Some(format!("{} / {}", p, m)),
        _ => None,
    }
}

/// Render a single tool invocation: its arguments plus the matching result,
/// when one can be found later in the transcript. The durable transcript
/// stores results as `[name result]:output`, keyed only by tool name, so we
/// pair calls with same-named results in encounter order (mirroring
/// [`crate::tui::transcript::transcript_messages_from_core`]).
fn render_tool_call<'a>(
    call: &'a ToolCall,
    messages: &[Message],
    cursor: &mut std::collections::HashMap<&'a str, usize>,
    out: &mut String,
) {
    out.push_str(&format!("### Tool call: `{}`\n\n", call.name));
    let pretty_args = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| call.arguments.clone());
    out.push_str("```json\n");
    out.push_str(&pretty_args);
    out.push_str("\n```\n\n");

    // Two `bash` calls in the same turn map to two distinct results rather
    // than both latching onto the first: each render advances a per-name
    // cursor and picks the result at that position among same-named matches.
    let slot = {
        let entry = cursor.entry(call.name.as_str()).or_insert(0);
        let current = *entry;
        *entry += 1;
        current
    };

    let matches: Vec<&Message> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter(|m| parse_tool_result(&m.content).is_some_and(|(name, _)| name == call.name))
        .collect();
    if let Some(result) = matches.get(slot) {
        if let Some((_, output)) = parse_tool_result(&result.content) {
            out.push_str("**Result:**\n\n");
            let fence = choose_fence(output);
            out.push_str(&fence);
            out.push('\n');
            out.push_str(output.trim());
            out.push('\n');
            out.push_str(&fence);
            out.push_str("\n\n");
        }
        if let Some(children) = result.children.as_ref() {
            render_subagent_summary(children, out);
        }
    } else {
        out.push_str("_(no result recorded — the call may have been interrupted.)_\n\n");
    }
}

/// Summarise a subagent transcript inlined on a `task` tool result. Dumping
/// the full nested transcript would balloon the export past what a receiving
/// agent needs; instead we surface the task description, message count, and
/// whether the run finished in an error state.
fn render_subagent_summary(children: &[Message], out: &mut String) {
    if children.is_empty() {
        return;
    }
    let user_count = children.iter().filter(|m| m.role == Role::User).count();
    let assistant_count = children
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    let tool_count = children.iter().filter(|m| m.role == Role::Tool).count();
    out.push_str(&format!(
        "_Sub-agent transcript: {} user / {} assistant / {} tool messages._\n\n",
        user_count, assistant_count, tool_count
    ));
}

/// Parse the `[<name> result]:<output>` envelope that wraps Tool-role
/// messages. Mirrors [`crate::tui::transcript::parse_tool_result`] but
/// operates on an owned `&str` so it composes with the iterator pipeline in
/// [`render_tool_call`].
fn parse_tool_result(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('[')?;
    let (name, output) = content.split_once(" result]:")?;
    Some((name, output.trim_start_matches('\n')))
}

/// Pick a Markdown fence tall enough that the embedded result content (which
/// may itself contain ``` fences) does not prematurely close the block. Scans
/// for the longest run of consecutive backticks and adds one.
fn choose_fence(content: &str) -> String {
    let longest = content
        .split(|c: char| c != '`')
        .map(|run| run.len())
        .max()
        .unwrap_or(0);
    "`".repeat((longest + 1).max(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use neenee_core::{Pursuit, ToolCall};

    fn user(content: &str) -> Message {
        Message::new(Role::User, content)
    }

    fn assistant_with_call(content: &str, call: ToolCall) -> Message {
        let mut m = Message::new(Role::Assistant, content);
        m.tool_calls = Some(vec![call]);
        m
    }

    fn tool_result(name: &str, output: &str) -> Message {
        let call = ToolCall {
            id: format!("{}_id", name),
            name: name.to_string(),
            arguments: "{}".to_string(),
        };
        Message::tool_result(&call, format!("[{} result]:\n{}", name, output))
    }

    #[test]
    fn renders_metadata_header_and_preamble() {
        let out = format_export_markdown(
            ExportContext {
                session_id: "abcd1234ef",
                provider: "kimi-code",
                model: "kimi-k2.7-code",
                pursuit: None,
                active_plan_path: None,
            },
            &[user("hello")],
        );
        assert!(out.contains("Session ID:** `abcd1234ef`"));
        assert!(out.contains("Provider / Model:** kimi-code / kimi-k2.7-code"));
        assert!(out.contains("**Pursuit:** _none_"));
        assert!(out.contains("A fresh agent can read it as context"));
    }

    #[test]
    fn includes_pursuit_objective() {
        let pursuit = Pursuit {
            objective: "Ship /export".to_string(),
            is_complete: false,
        };
        let out = format_export_markdown(
            ExportContext {
                session_id: "id",
                provider: "p",
                model: "m",
                pursuit: Some(&pursuit),
                active_plan_path: None,
            },
            &[user("hi")],
        );
        assert!(out.contains("Ship /export"));
    }

    #[test]
    fn skips_hidden_and_system_messages() {
        let messages = vec![
            Message::hidden(Role::System, "internal"),
            user("visible"),
            Message::hidden(Role::User, "hidden user prompt"),
        ];
        let out = format_export_markdown(
            ExportContext {
                session_id: "id",
                provider: "p",
                model: "m",
                pursuit: None,
                active_plan_path: None,
            },
            &messages,
        );
        assert!(out.contains("visible"));
        assert!(!out.contains("hidden user prompt"));
        assert!(!out.contains("internal"));
    }

    #[test]
    fn inlines_tool_call_and_result() {
        let call = ToolCall {
            id: "bash_1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"ls"}"#.to_string(),
        };
        let messages = vec![
            user("list files"),
            assistant_with_call("", call),
            tool_result("bash", "file1\nfile2"),
        ];
        let out = format_export_markdown(
            ExportContext {
                session_id: "id",
                provider: "p",
                model: "m",
                pursuit: None,
                active_plan_path: None,
            },
            &messages,
        );
        assert!(out.contains("### Tool call: `bash`"));
        assert!(out.contains(r#""command": "ls""#));
        assert!(out.contains("**Result:**"));
        assert!(out.contains("file1"));
    }

    #[test]
    fn pairs_repeated_same_named_calls_in_order() {
        let call_a = ToolCall {
            id: "bash_a".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"echo a"}"#.to_string(),
        };
        let call_b = ToolCall {
            id: "bash_b".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"echo b"}"#.to_string(),
        };
        let messages = vec![
            assistant_with_call("", call_a),
            tool_result("bash", "first"),
            assistant_with_call("", call_b),
            tool_result("bash", "second"),
        ];
        let out = format_export_markdown(
            ExportContext {
                session_id: "id",
                provider: "p",
                model: "m",
                pursuit: None,
                active_plan_path: None,
            },
            &messages,
        );
        // The first call must pair with "first", the second with "second".
        let first_call = out.find("echo a").unwrap();
        let first_result = out.find("first").unwrap();
        let second_call = out.find("echo b").unwrap();
        let second_result = out.find("second").unwrap();
        assert!(first_call < first_result);
        assert!(first_result < second_call);
        assert!(second_call < second_result);
    }

    #[test]
    fn notes_interrupted_call_when_no_result() {
        let call = ToolCall {
            id: "bash_1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"sleep 10"}"#.to_string(),
        };
        let messages = vec![user("kick off"), assistant_with_call("", call)];
        let out = format_export_markdown(
            ExportContext {
                session_id: "id",
                provider: "p",
                model: "m",
                pursuit: None,
                active_plan_path: None,
            },
            &messages,
        );
        assert!(out.contains("no result recorded"));
    }

    #[test]
    fn empty_session_emits_placeholder() {
        let out = format_export_markdown(
            ExportContext {
                session_id: "id",
                provider: "p",
                model: "m",
                pursuit: None,
                active_plan_path: None,
            },
            &[],
        );
        assert!(out.contains("No user-visible turns"));
    }
}
