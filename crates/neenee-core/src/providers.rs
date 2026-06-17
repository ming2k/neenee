use crate::{retryable_error, Message, Provider, ProviderStreamEvent, Role, Tool, ToolCall};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

const NEENEE_USER_AGENT: &str = concat!("neenee/", env!("CARGO_PKG_VERSION"));

fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
    {
        return Some(milliseconds.max(0.0) as u64);
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<f64>() {
        return Some((seconds.max(0.0) * 1000.0) as u64);
    }
    let parsed = httpdate::parse_http_date(value).ok()?;
    let now = SystemTime::now();
    Some(
        parsed
            .duration_since(now)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
    )
}

async fn ensure_success(
    response: reqwest::Response,
    provider: &str,
) -> Result<reqwest::Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after_ms(response.headers());
    let body = response.text().await.unwrap_or_default();
    let message = format!("{} HTTP {}: {}", provider, status, body);
    if status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error() {
        Err(retryable_error(message, retry_after))
    } else {
        Err(message)
    }
}

fn transport_error(provider: &str, error: reqwest::Error) -> String {
    let message = format!("{} transport error: {}", provider, error);
    if error.is_timeout() || error.is_connect() || error.is_request() {
        retryable_error(message, None)
    } else {
        message
    }
}

pub struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Ok(Message {
            role: Role::Assistant,
            content: "Hello! I am a mock AI. How can I help you today?".to_string(),
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            hidden: false,
        })
    }

    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
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
    pub user_agent: String,
    tools: Mutex<Option<Vec<Value>>>,
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_base_url(api_key, model, "https://api.openai.com/v1/chat/completions")
    }

    pub fn with_base_url(api_key: String, model: String, base_url: &str) -> Self {
        Self::with_base_url_and_user_agent(api_key, model, base_url, NEENEE_USER_AGENT)
    }

    pub fn with_base_url_and_user_agent(
        api_key: String,
        model: String,
        base_url: &str,
        user_agent: &str,
    ) -> Self {
        Self {
            api_key,
            model,
            base_url: base_url.to_string(),
            user_agent: user_agent.to_string(),
            tools: Mutex::new(None),
        }
    }

    fn request_body(&self, messages: Vec<Message>, stream: bool) -> Value {
        let tools = self.tools.lock().unwrap();
        // OpenAI rejects any `tool` message whose `tool_call_id` does not match
        // a `tool_call` on a preceding assistant message. Drop orphan tool
        // results (e.g. from text-fallback calls or older saved sessions) so the
        // request can never fail with "tool_call_id is not found".
        let mut known_ids = std::collections::HashSet::new();
        let messages: Vec<Message> = messages
            .into_iter()
            .filter(|message| {
                if !valid_provider_message(message) {
                    return false;
                }
                match message.role {
                    Role::Assistant => {
                        if let Some(calls) = &message.tool_calls {
                            for call in calls {
                                known_ids.insert(call.id.clone());
                            }
                        }
                        true
                    }
                    Role::Tool => message
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| !id.is_empty() && known_ids.contains(id)),
                    _ => true,
                }
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "stream": stream,
            "messages": messages
                .into_iter()
                .map(openai_message)
                .collect::<Vec<_>>()
        });
        if let Some(tools) = tools.as_ref().filter(|tools| !tools.is_empty()) {
            body["tools"] = json!(tools);
            body["tool_choice"] = "auto".into();
        }
        body
    }
}

fn valid_provider_message(message: &Message) -> bool {
    message.role != Role::Assistant
        || !message.content.is_empty()
        || message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
}

fn openai_message(m: Message) -> Value {
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
        map["tool_calls"] = json!(tool_calls
            .into_iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {"name": tc.name, "arguments": tc.arguments}
                })
            })
            .collect::<Vec<_>>());
    }
    if let Some(tool_call_id) = m.tool_call_id {
        map["tool_call_id"] = json!(tool_call_id);
    }
    map
}

fn parse_openai_stream_data(data: &str) -> Vec<ProviderStreamEvent> {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    let delta = &value["choices"][0]["delta"];
    let mut events = Vec::new();
    if let Some(content) = delta["content"].as_str().filter(|value| !value.is_empty()) {
        events.push(ProviderStreamEvent::TextDelta(content.to_string()));
    }
    if let Some(reasoning) = delta["reasoning_content"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        events.push(ProviderStreamEvent::ReasoningDelta(reasoning.to_string()));
    }
    if let Some(tool_calls) = delta["tool_calls"].as_array() {
        for call in tool_calls {
            events.push(ProviderStreamEvent::ToolCallDelta {
                index: call["index"].as_u64().unwrap_or(0) as usize,
                id: call["id"].as_str().map(str::to_string),
                name: call["function"]["name"].as_str().map(str::to_string),
                arguments: call["function"]["arguments"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    events
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        let schemas: Vec<Value> = tools.iter().map(|t| t.to_openai_function()).collect();
        let _ = self.tools.lock().map(|mut guard| {
            *guard = Some(schemas);
        });
    }

    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let client = reqwest::Client::new();

        let body = self.request_body(messages, false);

        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("OpenAI", error))?;
        let response = ensure_success(response, "OpenAI").await?;

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(err) = response_json.get("error") {
            return Err(format!("OpenAI Error: {}", err));
        }

        let choice = &response_json["choices"][0]["message"];
        let content = choice["content"].as_str().unwrap_or("").to_string();
        let reasoning_content = choice["reasoning_content"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        let tool_calls = choice.get("tool_calls").and_then(|tc| {
            tc.as_array().map(|arr| {
                arr.iter()
                    .map(|t| ToolCall {
                        id: t["id"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                        name: t["function"]["name"].as_str().unwrap_or("").to_string(),
                        arguments: t["function"]["arguments"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect()
            })
        });

        Ok(Message {
            role: Role::Assistant,
            content,
            display_content: None,
            reasoning_content,
            tool_calls,
            tool_call_id: None,
            hidden: false,
        })
    }

    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let client = reqwest::Client::new();

        let body = self.request_body(messages, true);

        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("OpenAI", error))?;
        let response = ensure_success(response, "OpenAI").await?;

        let mut buffer = String::new();
        let stream = response.bytes_stream().map(move |item| match item {
            Ok(bytes) => {
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                let mut content = String::new();

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer.drain(..pos + 1);

                    if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
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
            Err(error) => Err(transport_error("OpenAI", error)),
        });

        Ok(stream.boxed())
    }

    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        let response = reqwest::Client::new()
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&self.request_body(messages, true))
            .send()
            .await
            .map_err(|error| transport_error("OpenAI", error))?;
        let response = ensure_success(response, "OpenAI").await?;

        let mut buffer = String::new();
        Ok(response
            .bytes_stream()
            .map(move |item| {
                let bytes = item.map_err(|error| transport_error("OpenAI", error))?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                let mut events = Vec::new();
                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer.drain(..pos + 1);
                    if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
                        if data != "[DONE]" {
                            events.extend(parse_openai_stream_data(data).into_iter().map(Ok));
                        }
                    }
                }
                Ok::<_, String>(events)
            })
            .flat_map(|result| match result {
                Ok(events) => futures::stream::iter(events),
                Err(error) => futures::stream::iter(vec![Err(error)]),
            })
            .boxed())
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

fn gemini_request_body(messages: Vec<Message>) -> Value {
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

        if let Some(previous) = contents.last_mut() {
            if previous.get("role").and_then(Value::as_str) == Some(role) {
                if let Some(parts) = previous.get_mut("parts").and_then(Value::as_array_mut) {
                    parts.push(json!({ "text": text }));
                    continue;
                }
            }
        }
        contents.push(json!({
            "role": role,
            "parts": [{ "text": text }]
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            hidden: false,
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

        let stream = response.bytes_stream().map(|item| match item {
            Ok(bytes) => {
                let s = String::from_utf8_lossy(&bytes);
                let mut content = String::new();
                for line in s.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array())
                            {
                                if !candidates.is_empty() {
                                    if let Some(parts) =
                                        candidates[0]["content"]["parts"].as_array()
                                    {
                                        for part in parts {
                                            if let Some(text) =
                                                part.get("text").and_then(|t| t.as_str())
                                            {
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
            Err(error) => Err(transport_error("Gemini", error)),
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
                    "content": m.content,
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            hidden: false,
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
                    "content": m.content,
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

        let mut buffer = String::new();
        let stream = response.bytes_stream().map(move |item| match item {
            Ok(bytes) => {
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                let mut content = String::new();

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer.drain(..pos + 1);

                    if let Some(data) = line.strip_prefix("data: ") {
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
            Err(error) => Err(transport_error("LlamaServer", error)),
        });

        Ok(stream.boxed())
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// OpenAI-compatible provider wrappers for popular Chinese & global services
// ═════════════════════════════════════════════════════════════════════════════

/// Kimi Code — OpenAI-compatible coding-agent endpoint.
/// Endpoint: https://api.kimi.com/coding/v1/chat/completions
/// Env: `KIMI_CODE_API_KEY`
/// The fixed model ID is mapped to the latest coding model by Kimi Code.
pub struct KimiCodeProvider(OpenAIProvider);

impl KimiCodeProvider {
    pub const MODEL: &'static str = "kimi-for-coding";
    pub const ENDPOINT: &'static str = "https://api.kimi.com/coding/v1/chat/completions";
    pub const OPENCODE_USER_AGENT: &'static str = "opencode/1.17.4";

    pub fn new(api_key: String, user_agent: String) -> Self {
        Self(OpenAIProvider::with_base_url_and_user_agent(
            api_key,
            Self::MODEL.to_string(),
            Self::ENDPOINT,
            &user_agent,
        ))
    }
}

#[async_trait]
impl Provider for KimiCodeProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        self.0.prepare_tools(tools);
    }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        self.0.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        self.0.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        self.0.stream_chat_events(messages).await
    }
}

/// Kimi Open Platform (Moonshot AI) — OpenAI-compatible endpoint.
/// Base URL: https://api.moonshot.cn/v1/chat/completions
/// Env: `KIMI_API_KEY`
/// Popular models: moonshot-v1-8k, moonshot-v1-32k, moonshot-v1-128k,
///                  moonshot-v1-8k-vision-preview
pub struct KimiProvider(OpenAIProvider);

impl KimiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://api.moonshot.cn/v1/chat/completions",
        ))
    }
}

#[async_trait]
impl Provider for KimiProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        self.0.prepare_tools(tools);
    }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        self.0.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        self.0.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        self.0.stream_chat_events(messages).await
    }
}

/// DeepSeek — OpenAI-compatible endpoint.
/// Base URL: https://api.deepseek.com/v1/chat/completions
/// Env: `DEEPSEEK_API_KEY`
/// Popular models: deepseek-chat, deepseek-reasoner
/// Note: deepseek-reasoner returns `reasoning_content` field.
pub struct DeepSeekProvider(OpenAIProvider);

impl DeepSeekProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://api.deepseek.com/v1/chat/completions",
        ))
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        self.0.prepare_tools(tools);
    }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        self.0.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        self.0.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        self.0.stream_chat_events(messages).await
    }
}

/// Qwen (Tongyi / Alibaba DashScope) — OpenAI-compatible endpoint.
/// Base URL: https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions
/// Env: `DASHSCOPE_API_KEY`
/// Popular models: qwen-plus, qwen-max, qwen-turbo, qwen-coder-plus
/// International users: https://dashscope-intl.aliyuncs.com/compatible-mode/v1
pub struct QwenProvider(OpenAIProvider);

impl QwenProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        ))
    }

    pub fn new_intl(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions",
        ))
    }
}

#[async_trait]
impl Provider for QwenProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        self.0.prepare_tools(tools);
    }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        self.0.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        self.0.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        self.0.stream_chat_events(messages).await
    }
}

/// GLM (Zhipu AI / 智谱) — OpenAI-compatible endpoint.
/// Base URL: https://open.bigmodel.cn/api/paas/v4/chat/completions
/// Env: `GLM_API_KEY`
/// Popular models: glm-4-plus, glm-4, glm-4-air, glm-4-flash, glm-4v
pub struct GLMProvider(OpenAIProvider);

impl GLMProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        ))
    }
}

#[async_trait]
impl Provider for GLMProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        self.0.prepare_tools(tools);
    }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        self.0.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        self.0.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        self.0.stream_chat_events(messages).await
    }
}

/// Volcengine (火山引擎 / ByteDance Ark) — OpenAI-compatible endpoint.
/// Base URL: https://ark.cn-beijing.volces.com/api/v3/chat/completions
/// Env: `VOLCENGINE_API_KEY`
/// Popular models: deepseek-v3-250324, deepseek-r1-250324, doubao-pro-256k
pub struct VolcengineProvider(OpenAIProvider);

impl VolcengineProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://ark.cn-beijing.volces.com/api/v3/chat/completions",
        ))
    }
}

#[async_trait]
impl Provider for VolcengineProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        self.0.prepare_tools(tools);
    }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        self.0.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        self.0.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        self.0.stream_chat_events(messages).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_preserves_system_harness_context() {
        let body = gemini_request_body(vec![
            Message::new(Role::System, "goal and tools"),
            Message::new(Role::User, "continue"),
        ]);

        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "goal and tools"
        );
        assert_eq!(body["contents"][0]["role"], "user");
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

    #[test]
    fn openai_stream_parser_preserves_tool_call_fragments() {
        let events = parse_openai_stream_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_","arguments":"{\"pa"}}]}}]}"#,
        );
        assert_eq!(
            events,
            vec![ProviderStreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("read_".to_string()),
                arguments: "{\"pa".to_string(),
            }]
        );
    }

    #[test]
    fn openai_request_filters_empty_assistant_history() {
        let provider = OpenAIProvider::new("test-key".to_string(), "test-model".to_string());
        let body = provider.request_body(
            vec![
                Message::new(Role::User, "hello"),
                Message::new(Role::Assistant, ""),
                Message::new(Role::User, "again"),
            ],
            true,
        );

        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][1]["content"], "again");
    }

    #[test]
    fn openai_request_drops_orphan_tool_results() {
        let provider = OpenAIProvider::new("test-key".to_string(), "test-model".to_string());
        let matched = ToolCall {
            id: "call_matched".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        };
        let assistant_with_call = Message {
            role: Role::Assistant,
            content: String::new(),
            display_content: None,
            reasoning_content: None,
            tool_calls: Some(vec![matched.clone()]),
            tool_call_id: None,
            hidden: false,
        };
        let good_result = Message {
            role: Role::Tool,
            content: "ok".to_string(),
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some("call_matched".to_string()),
            hidden: false,
        };
        let orphan_result = Message {
            tool_call_id: Some("call_orphan".to_string()),
            ..Message::new(Role::Tool, "orphan")
        };
        let empty_id_result = Message {
            tool_call_id: Some(String::new()),
            ..Message::new(Role::Tool, "empty id")
        };

        let body = provider.request_body(
            vec![
                Message::new(Role::User, "hi"),
                assistant_with_call,
                good_result,
                orphan_result,
                empty_id_result,
            ],
            false,
        );

        let messages = body["messages"].as_array().unwrap();
        // user, assistant(tool_calls), and only the matched tool result survive.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_matched");
    }

    #[test]
    fn kimi_code_uses_fixed_coding_endpoint_and_model() {
        let provider = KimiCodeProvider::new(
            "test-key".to_string(),
            KimiCodeProvider::OPENCODE_USER_AGENT.to_string(),
        );

        assert_eq!(provider.0.base_url, KimiCodeProvider::ENDPOINT);
        assert_eq!(provider.0.model, KimiCodeProvider::MODEL);
        assert_eq!(provider.0.user_agent, KimiCodeProvider::OPENCODE_USER_AGENT);
    }

    #[test]
    fn retry_after_supports_seconds_and_milliseconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "2.5".parse().unwrap());
        assert_eq!(retry_after_ms(&headers), Some(2_500));

        headers.insert("retry-after-ms", "750".parse().unwrap());
        assert_eq!(retry_after_ms(&headers), Some(750));
    }
}
