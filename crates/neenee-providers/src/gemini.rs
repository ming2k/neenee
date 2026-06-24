//! Google Gemini native provider (REST to `generativelanguage.googleapis.com`).

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use neenee_core::{Message, Provider, Role};
use serde_json::{json, Value};

use crate::{ensure_success, transport_error};

pub struct GeminiProvider {
    pub api_key: String,
    pub model: String,
    pub id: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            id: "gemini".to_string(),
        }
    }
}

pub(crate) fn gemini_request_body(messages: Vec<Message>) -> Value {
    let mut system = Vec::new();
    let mut contents: Vec<Value> = Vec::new();

    for message in messages {
        if message.role == Role::System {
            system.push(message.content);
            continue;
        }

        let role = if message.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let text = if message.role == Role::Tool {
            format!("[tool result]\n{}", message.content)
        } else {
            message.content
        };
        let images = message.images.unwrap_or_default();

        // Build the parts for this message. When there are no images we keep
        // the original behaviour of always emitting a single text part (even
        // when empty, e.g. for tool-call-only assistant turns). With images we
        // emit the text part only when non-empty, followed by inline_data parts.
        let mut new_parts: Vec<Value> = Vec::new();
        if images.is_empty() {
            new_parts.push(json!({ "text": text }));
        } else {
            if !text.is_empty() {
                new_parts.push(json!({ "text": text }));
            }
            for image in &images {
                new_parts.push(json!({
                    "inline_data": {
                        "mime_type": image.mime,
                        "data": image.data,
                    }
                }));
            }
        }

        if let Some(previous) = contents.last_mut() {
            if previous.get("role").and_then(Value::as_str) == Some(role) {
                if let Some(parts) = previous.get_mut("parts").and_then(Value::as_array_mut) {
                    parts.extend(new_parts);
                    continue;
                }
            }
        }
        contents.push(json!({
            "role": role,
            "parts": new_parts
        }));
    }

    let mut body = json!({ "contents": contents });
    if !system.is_empty() {
        body["systemInstruction"] = json!({
            "parts": [{ "text": system.join("\n\n") }]
        });
    }
    body
}

#[async_trait]
impl Provider for GeminiProvider {
    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let client = reqwest::Client::new();
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let body = gemini_request_body(messages);

        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("Gemini", error))?;
        let response = ensure_success(response, "Gemini").await?;

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(err) = response_json.get("error") {
            return Err(format!("Gemini Error: {}", err));
        }

        let candidates = response_json
            .get("candidates")
            .and_then(|c| c.as_array())
            .ok_or_else(|| format!("Invalid Gemini response: {}", response_json))?;

        if candidates.is_empty() {
            return Err("Gemini returned no candidates".to_string());
        }

        let content_obj = &candidates[0]["content"];
        let parts = content_obj
            .get("parts")
            .and_then(|p| p.as_array())
            .ok_or_else(|| "Missing parts in Gemini response".to_string())?;

        let mut content_text = String::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                content_text.push_str(text);
            }
        }

        Ok(Message {
            role: Role::Assistant,
            content: content_text,
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            subagent_meta: None,
        })
    }

    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let client = reqwest::Client::new();
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            self.model, self.api_key
        );

        let body = gemini_request_body(messages);

        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("Gemini", error))?;
        let response = ensure_success(response, "Gemini").await?;

        // SSE byte reassembly (incl. multi-byte UTF-8 split across chunks) is
        // handled by `sse::data_payloads`; here we only map each payload to the
        // Gemini `streamGenerateContent` text shape.
        let stream = crate::sse::data_payloads(response, "Gemini")
            .map(|item| item.map(|payload| extract_text(&payload)));

        Ok(stream.boxed())
    }
}

/// Parse one `streamGenerateContent` SSE payload and concatenate the text from
/// `candidates[0].content.parts[].text`. Returns an empty string when the
/// payload carries no text part (e.g. a finish-reason-only chunk).
fn extract_text(payload: &str) -> String {
    let value: Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(_) => return String::new(),
    };
    value
        .get("candidates")
        .and_then(|candidates| candidates.as_array())
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate["content"]["parts"].as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default()
}

// Gemini relies on the `Provider::stream_chat_events` trait default because
// its REST surface does not emit tool-call deltas; the default wraps the text
// stream as `TextDelta`s, which is what the harness expects from this provider.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_preserves_system_harness_context() {
        let body = gemini_request_body(vec![
            Message::new(Role::System, "pursuit and tools"),
            Message::new(Role::User, "continue"),
        ]);

        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "pursuit and tools"
        );
        assert_eq!(body["contents"][0]["role"], "user");
    }

    #[test]
    fn extract_text_concatenates_parts_and_preserves_cjk() {
        let payload = r#"{"candidates":[{"content":{"parts":[{"text":"并行"},{"text":"开发"}]}}]}"#;
        assert_eq!(extract_text(payload), "并行开发");
    }

    #[test]
    fn extract_text_returns_empty_for_non_text_payload() {
        assert_eq!(
            extract_text(r#"{"candidates":[{"finishReason":"STOP"}]}"#),
            ""
        );
        assert_eq!(extract_text("not json"), "");
    }

    #[test]
    fn gemini_fallback_tool_results_are_user_context() {
        let body = gemini_request_body(vec![
            Message::new(Role::Assistant, "{\"tool\":\"read_file\"}"),
            Message::new(Role::Tool, "file contents"),
            Message::new(Role::User, "next"),
        ]);

        assert_eq!(body["contents"][1]["role"], "user");
        assert_eq!(
            body["contents"][1]["parts"][0]["text"],
            "[tool result]\nfile contents"
        );
        assert_eq!(body["contents"][1]["parts"][1]["text"], "next");
    }
}
