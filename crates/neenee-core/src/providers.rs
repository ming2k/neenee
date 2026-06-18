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
            images: None,
            provider: None,
            model: None,
            hidden: false,
        })
    }

    fn provider_id(&self) -> String {
        "mock".to_string()
    }

    fn model(&self) -> String {
        "mock".to_string()
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
    /// Stable provider/solution id surfaced via [`Provider::provider_id`] so
    /// assistant messages can be attributed. Defaults to `"openai"`; the
    /// OpenAI-compatible registry overrides it to the preset id (e.g.
    /// `"kimi-code"`) in [`OpenAiCompatProvider::build`].
    pub id: String,
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
            id: "openai".to_string(),
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

fn openai_content(m: &Message) -> Value {
    match &m.images {
        Some(images) if !images.is_empty() => {
            let mut parts = Vec::new();
            if !m.content.is_empty() {
                parts.push(json!({ "type": "text", "text": m.content }));
            }
            for image in images {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", image.mime, image.data)
                    }
                }));
            }
            Value::Array(parts)
        }
        _ => Value::String(m.content.clone()),
    }
}

fn openai_message(m: Message) -> Value {
    let mut map = json!({
        "role": match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        },
        "content": openai_content(&m),
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

    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn model(&self) -> String {
        self.model.clone()
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
            images: None,
            provider: None,
            model: None,
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
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

/// The Kimi coding endpoint authenticates clients by a fixed user agent that
/// impersonates the OpenCode client; it is the default unless overridden.
pub const KIMI_CODE_USER_AGENT: &str = "opencode/1.17.4";

/// Specification for an OpenAI-compatible provider.
///
/// Every provider in [`OPENAI_COMPAT_PROVIDERS`] speaks the OpenAI
/// chat-completions wire format and differs only in endpoint, default model,
/// the environment variables consulted, and (rarely) a pinned model or a
/// required user agent. Modelling them as *data* rather than one delegating
/// newtype per vendor means adding a provider is a single table entry instead
/// of ~30 lines of boilerplate trait delegation.
pub struct OpenAiCompatProvider {
    /// Stable identifier used in config (`default_provider`) and the TUI.
    pub id: &'static str,
    /// Full chat-completions endpoint URL.
    pub base_url: &'static str,
    /// Model used when neither config nor environment specifies one.
    pub default_model: &'static str,
    /// Environment variable consulted for the API key.
    pub env_api_key: &'static str,
    /// Environment variable consulted for a model override.
    pub env_model: &'static str,
    /// When set, the endpoint pins this model and ignores any override
    /// (e.g. the Kimi coding endpoint).
    pub fixed_model: Option<&'static str>,
    /// When set, the endpoint requires this user agent unless overridden.
    pub default_user_agent: Option<&'static str>,
}

/// The single registry of OpenAI-compatible providers — the source of truth for
/// their endpoints, default models, and environment variables.
pub const OPENAI_COMPAT_PROVIDERS: &[OpenAiCompatProvider] = &[
    // Kimi Code — OpenAI-compatible coding-agent endpoint. The fixed model ID
    // is mapped to the latest coding model by Kimi Code.
    OpenAiCompatProvider {
        id: "kimi-code",
        base_url: "https://api.kimi.com/coding/v1/chat/completions",
        default_model: "kimi-for-coding",
        env_api_key: "KIMI_CODE_API_KEY",
        env_model: "KIMI_CODE_MODEL",
        fixed_model: Some("kimi-for-coding"),
        default_user_agent: Some(KIMI_CODE_USER_AGENT),
    },
    // Kimi Open Platform (Moonshot AI). Models: moonshot-v1-{8k,32k,128k}.
    OpenAiCompatProvider {
        id: "kimi",
        base_url: "https://api.moonshot.cn/v1/chat/completions",
        default_model: "moonshot-v1-8k",
        env_api_key: "KIMI_API_KEY",
        env_model: "KIMI_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // DeepSeek. Models: deepseek-chat, deepseek-reasoner (returns reasoning_content).
    OpenAiCompatProvider {
        id: "deepseek",
        base_url: "https://api.deepseek.com/v1/chat/completions",
        default_model: "deepseek-chat",
        env_api_key: "DEEPSEEK_API_KEY",
        env_model: "DEEPSEEK_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // Qwen (Tongyi / Alibaba DashScope). Models: qwen-plus, qwen-max, qwen-coder-plus.
    OpenAiCompatProvider {
        id: "qwen",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        default_model: "qwen-plus",
        env_api_key: "DASHSCOPE_API_KEY",
        env_model: "QWEN_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // GLM (Zhipu AI / 智谱). Models: glm-4-plus, glm-4, glm-4-air, glm-4-flash.
    OpenAiCompatProvider {
        id: "glm",
        base_url: "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        default_model: "glm-4-plus",
        env_api_key: "GLM_API_KEY",
        env_model: "GLM_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // Volcengine (火山引擎 / ByteDance Ark). Models: deepseek-v3-250324, doubao-pro-256k.
    OpenAiCompatProvider {
        id: "volcengine",
        base_url: "https://ark.cn-beijing.volces.com/api/v3/chat/completions",
        default_model: "deepseek-v3-250324",
        env_api_key: "VOLCENGINE_API_KEY",
        env_model: "VOLCENGINE_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
];

/// Look up an OpenAI-compatible provider spec by its identifier.
pub fn openai_compat_provider(id: &str) -> Option<&'static OpenAiCompatProvider> {
    OPENAI_COMPAT_PROVIDERS.iter().find(|spec| spec.id == id)
}

impl OpenAiCompatProvider {
    /// Resolve the model to use: a pinned `fixed_model` always wins, otherwise
    /// the caller's override, otherwise the provider default.
    pub fn resolve_model(&self, override_model: Option<String>) -> String {
        if let Some(fixed) = self.fixed_model {
            return fixed.to_string();
        }
        override_model.unwrap_or_else(|| self.default_model.to_string())
    }

    /// Build a concrete [`OpenAIProvider`] for this spec. `user_agent` overrides
    /// the spec default (used by the Kimi coding endpoint).
    pub fn build(
        &self,
        api_key: String,
        override_model: Option<String>,
        user_agent: Option<String>,
    ) -> OpenAIProvider {
        let model = self.resolve_model(override_model);
        let agent = user_agent
            .or_else(|| self.default_user_agent.map(str::to_string))
            .unwrap_or_else(|| NEENEE_USER_AGENT.to_string());
        let mut provider = OpenAIProvider::with_base_url_and_user_agent(
            api_key,
            model,
            self.base_url,
            &agent,
        );
        provider.id = self.id.to_string();
        provider
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
            images: None,
            provider: None,
            model: None,
            hidden: false,
        };
        let good_result = Message {
            role: Role::Tool,
            content: "ok".to_string(),
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some("call_matched".to_string()),
            images: None,
            provider: None,
            model: None,
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
        let spec = openai_compat_provider("kimi-code").expect("kimi-code spec");
        // A pinned model ignores any caller override.
        assert_eq!(spec.resolve_model(Some("ignored".to_string())), "kimi-for-coding");

        let provider = spec.build("test-key".to_string(), None, None);
        assert_eq!(provider.base_url, spec.base_url);
        assert_eq!(provider.model, "kimi-for-coding");
        assert_eq!(provider.user_agent, KIMI_CODE_USER_AGENT);
        // The registry stamps the preset id onto the concrete provider so
        // assistant responses can be attributed to "kimi-code".
        assert_eq!(provider.id, "kimi-code");
        assert_eq!(provider.provider_id(), "kimi-code");
        assert_eq!(provider.model(), "kimi-for-coding");
    }

    #[test]
    fn openai_compat_spec_resolves_model_override_and_default() {
        let spec = openai_compat_provider("deepseek").expect("deepseek spec");
        assert_eq!(spec.resolve_model(None), "deepseek-chat");
        assert_eq!(
            spec.resolve_model(Some("deepseek-reasoner".to_string())),
            "deepseek-reasoner"
        );
        // Non-coding providers fall back to the shared neenee user agent.
        let provider = spec.build("k".to_string(), None, None);
        assert_eq!(provider.user_agent, NEENEE_USER_AGENT);
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
