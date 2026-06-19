//! Parsing of text-emitted tool calls.
//!
//! Providers without native function calling (or ones that mirror a native
//! call as text) emit a JSON object such as `{"tool":"bash","arguments":{...}}`,
//! optionally wrapped in ChatML/Hermes sentinel tokens.
//! [`parse_text_tool_call`] recovers a [`ToolCall`] from such prose-embedded
//! JSON, and [`find_balanced_json_object`] is the shared brace-scanner both it
//! and the provider echo-filter build on.

use crate::{Message, Role, ToolCall};

/// Given the byte index of an opening `{` in `text`, return the byte index of
/// the matching closing `}` at the same nesting depth. String literals and
/// escapes are respected, so braces inside strings do not affect nesting.
/// Returns `None` if the braces never balance.
pub(crate) fn find_balanced_json_object(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &byte) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                in_str = false;
            }
        } else if byte == b'"' {
            in_str = true;
        } else if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Parse a tool call from assistant response text.
///
/// Supports JSON tool calls emitted as plain text by providers without native
/// function calling. Robust to surrounding prose, markdown code fences, and
/// ChatML/Hermes-style special tokens (e.g. `<|tool_calls_section_end|>`,
/// `<tool_call>`): the first balanced `{ ... }` object carrying a recognised
/// tool identifier is used, so any text around the JSON is ignored. Both the
/// `"tool"` key and the OpenAI/MCP `"name"` key are accepted as the tool
/// identifier.
pub(crate) fn parse_text_tool_call(text: &str) -> Option<ToolCall> {
    let mut start = 0;
    while let Some(offset) = text[start..].find('{') {
        let brace_at = start + offset;
        if let Some(end) = find_balanced_json_object(text, brace_at) {
            let candidate = &text[brace_at..=end];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(candidate) {
                let tool_name = json
                    .get("tool")
                    .or_else(|| json.get("name"))
                    .and_then(|value| value.as_str());
                if let Some(tool_name) = tool_name {
                    let args = match json.get("arguments") {
                        Some(serde_json::Value::String(string)) => string.clone(),
                        Some(value) => value.to_string(),
                        None => "{}".to_string(),
                    };
                    return Some(ToolCall {
                        id: format!("call_{}", uuid::Uuid::new_v4()),
                        name: tool_name.to_string(),
                        arguments: args,
                    });
                }
            }
            // Skip past this object and keep searching; a later object in the
            // text may carry the tool identifier.
            start = end + 1;
        } else {
            // Unbalanced `{` with no matching close: nothing later can form a
            // complete object either, so stop.
            break;
        }
    }
    None
}

/// Promote a text-based (fallback) tool call onto the preceding assistant
/// message as a native `tool_calls` entry. This keeps the tool_call /
/// tool_call_id pairing valid for OpenAI-compatible providers (which require
/// every tool result to reference an assistant tool call), while non-native
/// providers simply ignore the `tool_calls` field and keep using the message
/// `content`.
pub(crate) fn attach_fallback_tool_call(messages: &mut [Message], call: &ToolCall) {
    if let Some(last) = messages.last_mut() {
        if last.role == Role::Assistant && last.tool_calls.is_none() {
            last.tool_calls = Some(vec![call.clone()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_tool_call_accepts_bare_json() {
        let call = parse_text_tool_call("{\"tool\":\"alpha\",\"arguments\":{\"k\":1}}").expect("bare json");
        assert_eq!(call.name, "alpha");
        assert_eq!(call.arguments, "{\"k\":1}");
    }

    #[test]
    fn parse_text_tool_call_ignores_trailing_special_tokens() {
        let call = parse_text_tool_call(
            "{\"tool\":\"read_file\",\"arguments\":{\"path\":\"x\"}}<|tool_calls_section_end|>",
        )
        .expect("trailing special token must not break parsing");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, "{\"path\":\"x\"}");
    }

    #[test]
    fn parse_text_tool_call_ignores_prose_and_code_fences() {
        let call = parse_text_tool_call(
            "I'll read it now.\n```json\n{\"name\":\"read\",\"arguments\":{}}\n```",
        )
        .expect("prose + fence should still be found");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, "{}");
    }

    #[test]
    fn parse_text_tool_call_accepts_name_key() {
        let call =
            parse_text_tool_call("{\"name\":\"alpha\"}").expect("name key is accepted");
        assert_eq!(call.name, "alpha");
        assert_eq!(call.arguments, "{}");
    }

    #[test]
    fn parse_text_tool_call_passes_through_string_arguments() {
        // Pre-serialised string arguments are forwarded verbatim, not
        // double-encoded by Value::to_string().
        let call = parse_text_tool_call("{\"tool\":\"alpha\",\"arguments\":\"{\\\"k\\\":1}\"}")
            .expect("string arguments");
        assert_eq!(call.arguments, "{\"k\":1}");
    }

    #[test]
    fn parse_text_tool_call_returns_none_for_plain_prose() {
        assert!(parse_text_tool_call("just some text, no tool call here").is_none());
    }

    #[test]
    fn parse_text_tool_call_skips_non_tool_json_objects() {
        // A JSON object without a tool/name key is skipped; a later object
        // carrying the identifier is still recognised.
        let call = parse_text_tool_call("{\"note\":\"thinking\"}{\"tool\":\"alpha\",\"arguments\":{}}")
            .expect("later object has the tool key");
        assert_eq!(call.name, "alpha");
    }
}
