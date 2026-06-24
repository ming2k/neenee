//! Local llama.cpp / llama-server HTTP provider.
//!
//! Speaks a minimal OpenAI-compatible subset (no tool calls, no streaming
//! tool-call delta reconstruction). Kept separate from `openai_compat` so the
//! full OpenAI chat-completions machinery does not leak into the simpler local
//! inference path.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use neenee_core::{Message, Provider, Role};
use serde_json::{json, Value};

use crate::{ensure_success, openai_compat::openai_content, transport_error};

pub struct LlamaServerProvider {
    pub base_url: String,
    pub model: String,
    pub id: String,
}

impl LlamaServerProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            id: "llama".to_string(),
        }
    }
}

#[async_trait]
impl Provider for LlamaServerProvider {
    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let client = reqwest::Client::new();
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let body = json!({
            "model": self.model,
            "messages": messages.into_iter().map(|m| {
                json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                        Role::Tool => "tool",
                    },
                    "content": openai_content(&m),
                })
            }).collect::<Vec<_>>()
        });

        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("LlamaServer", error))?;
        let response = ensure_success(response, "LlamaServer").await?;

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(err) = response_json.get("error") {
            return Err(format!("LlamaServer Error: {}", err));
        }

        let content = response_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(Message {
            role: Role::Assistant,
            content,
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
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let body = json!({
            "model": self.model,
            "stream": true,
            "messages": messages.into_iter().map(|m| {
                json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                        Role::Tool => "tool",
                    },
                    "content": openai_content(&m),
                })
            }).collect::<Vec<_>>()
        });

        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("LlamaServer", error))?;
        let response = ensure_success(response, "LlamaServer").await?;

        // SSE byte reassembly (incl. multi-byte UTF-8 split across chunks) is
        // handled by `sse::data_payloads`; here we only map each payload to the
        // OpenAI-compatible delta content shape.
        let stream = crate::sse::data_payloads(response, "LlamaServer").map(|item| {
            let data = item?;
            let mut content = String::new();
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(delta);
                }
            }
            Ok(content)
        });

        Ok(stream.boxed())
    }
}
