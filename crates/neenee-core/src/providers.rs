use crate::{Message, Provider, Role, ToolCall};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

pub struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Ok(Message {
            role: Role::Assistant,
            content: "Hello! I am a mock AI. How can I help you today?".to_string(),
            tool_calls: None,
        })
    }

    async fn stream_chat(&self, _messages: Vec<Message>) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let response = vec![
            Ok("This ".to_string()),
            Ok("is ".to_string()),
            Ok("a ".to_string()),
            Ok("streaming ".to_string()),
            Ok("mock ".to_string()),
            Ok("response ".to_string()),
            Ok("from ".to_string()),
            Ok("neenee!".to_string()),
        ];
        Ok(futures::stream::iter(response).boxed())
    }
}

pub struct OpenAIProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            base_url: "https://api.openai.com/v1/chat/completions".to_string(),
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let client = reqwest::Client::new();
        
        let body = json!({
            "model": self.model,
            "messages": messages.into_iter().map(|m| {
                let mut map = json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                        Role::Tool => "tool",
                    },
                    "content": m.content,
                });
                if let Some(tool_calls) = m.tool_calls {
                    map["tool_calls"] = json!(tool_calls.into_iter().map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments,
                            }
                        })
                    }).collect::<Vec<_>>());
                }
                map
            }).collect::<Vec<_>>()
        });

        let response = client.post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(err) = response_json.get("error") {
            return Err(format!("OpenAI Error: {}", err));
        }

        let choice = &response_json["choices"][0]["message"];
        let content = choice["content"].as_str().unwrap_or("").to_string();
        
        let tool_calls = choice.get("tool_calls").and_then(|tc| {
            tc.as_array().map(|arr| {
                arr.iter().map(|t| ToolCall {
                    id: t["id"].as_str().unwrap_or("").to_string(),
                    name: t["function"]["name"].as_str().unwrap_or("").to_string(),
                    arguments: t["function"]["arguments"].as_str().unwrap_or("").to_string(),
                }).collect()
            })
        });

        Ok(Message {
            role: Role::Assistant,
            content,
            tool_calls,
        })
    }

    async fn stream_chat(&self, messages: Vec<Message>) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let client = reqwest::Client::new();
        
        let body = json!({
            "model": self.model,
            "stream": true,
            "messages": messages.into_iter().map(|m| {
                let mut map = json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                        Role::Tool => "tool",
                    },
                    "content": m.content,
                });
                if let Some(tool_calls) = m.tool_calls {
                    map["tool_calls"] = json!(tool_calls.into_iter().map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments,
                            }
                        })
                    }).collect::<Vec<_>>());
                }
                map
            }).collect::<Vec<_>>()
        });

        let response = client.post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let mut buffer = String::new();
        let stream = response.bytes_stream().map(move |item| {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    let mut content = String::new();
                    
                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim().to_string();
                        buffer.drain(..pos + 1);
                        
                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                continue;
                            }
                            if let Ok(v) = serde_json::from_str::<Value>(data) {
                                if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                                    content.push_str(delta);
                                }
                            }
                        }
                    }
                    Ok(content)
                }
                Err(e) => Err(e.to_string()),
            }
        });

        Ok(stream.boxed())
    }
}

pub struct GeminiProvider {
    pub api_key: String,
    pub model: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self { api_key, model }
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let client = reqwest::Client::new();
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let body = json!({
            "contents": messages.into_iter().filter(|m| !matches!(m.role, Role::System)).map(|m| {
                json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "model",
                        Role::Tool => "function",
                        _ => "user",
                    },
                    "parts": [{ "text": m.content }]
                })
            }).collect::<Vec<_>>()
        });

        let response = client.post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(err) = response_json.get("error") {
            return Err(format!("Gemini Error: {}", err));
        }

        let candidates = response_json.get("candidates")
            .and_then(|c| c.as_array())
            .ok_or_else(|| format!("Invalid Gemini response: {}", response_json))?;
        
        if candidates.is_empty() {
            return Err("Gemini returned no candidates".to_string());
        }

        let content_obj = &candidates[0]["content"];
        let parts = content_obj.get("parts")
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
            tool_calls: None,
        })
    }

    async fn stream_chat(&self, messages: Vec<Message>) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let client = reqwest::Client::new();
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            self.model, self.api_key
        );

        let body = json!({
            "contents": messages.into_iter().filter(|m| !matches!(m.role, Role::System)).map(|m| {
                json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "model",
                        Role::Tool => "function",
                        _ => "user",
                    },
                    "parts": [{ "text": m.content }]
                })
            }).collect::<Vec<_>>()
        });

        let response = client.post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let stream = response.bytes_stream().map(|item| {
            match item {
                Ok(bytes) => {
                    let s = String::from_utf8_lossy(&bytes);
                    let mut content = String::new();
                    for line in s.lines() {
                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if let Ok(v) = serde_json::from_str::<Value>(data) {
                                if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) {
                                    if !candidates.is_empty() {
                                        if let Some(parts) = candidates[0]["content"]["parts"].as_array() {
                                            for part in parts {
                                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                                    content.push_str(text);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(content)
                }
                Err(e) => Err(e.to_string()),
            }
        });

        Ok(stream.boxed())
    }
}

pub struct LlamaServerProvider {
    pub base_url: String,
    pub model: String,
}

impl LlamaServerProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self { base_url, model }
    }
}

#[async_trait]
impl Provider for LlamaServerProvider {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));

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
                    "content": m.content,
                })
            }).collect::<Vec<_>>()
        });

        let response = client.post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

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
            tool_calls: None,
        })
    }

    async fn stream_chat(&self, messages: Vec<Message>) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));

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
                    "content": m.content,
                })
            }).collect::<Vec<_>>()
        });

        let response = client.post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let mut buffer = String::new();
        let stream = response.bytes_stream().map(move |item| {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    let mut content = String::new();
                    
                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim().to_string();
                        buffer.drain(..pos + 1);
                        
                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                continue;
                            }
                            if let Ok(v) = serde_json::from_str::<Value>(data) {
                                if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                                    content.push_str(delta);
                                }
                            }
                        }
                    }
                    Ok(content)
                }
                Err(e) => Err(e.to_string()),
            }
        });

        Ok(stream.boxed())
    }
}
