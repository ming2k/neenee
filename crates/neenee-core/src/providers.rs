use crate::{retryable_error, Message, Provider, ProviderStreamEvent, Role, Tool, ToolCall};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

pub const NEENEE_USER_AGENT: &str = concat!("neenee/", env!("CARGO_PKG_VERSION"));

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

pub struct OpenAiCompatProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub user_agent: String,
    /// Stable provider/solution id surfaced via [`Provider::provider_id`] so
    /// assistant messages can be attributed. Defaults to `"openai"`; the
    /// OpenAI-compatible registry overrides it to the preset id (e.g.
    /// `"kimi-code"`) in [`OpenAiProviderSpec::build`].
    pub id: String,
    tools: Mutex<Option<Vec<Value>>>,
}

impl OpenAiCompatProvider {
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

/// Sentinel tokens that wrap a tool call when a model mirrors it as text
/// content alongside native `tool_calls` (ChatML/Hermes/Qwen style), e.g.
/// `{"tool":"bash",...}<|tool_calls_section_end|>`.
const TOOL_CALL_SENTINELS: &[&str] = &[
    "<|tool_calls_section_begin|>",
    "<|tool_calls_section_end|>",
    "<|tool_calls_begin|>",
    "<|tool_calls_end|>",
    "<|tool_call_begin|>",
    "<|tool_call_end|>",
    "<|tool's_call_begin|>",
    "<|tool's_call_end|>",
    "<tool_call>",
    "</tool_call>",
];

/// Streaming filter that strips tool-call "echo" text from a content channel.
///
/// Models such as GLM/Qwen return native `tool_calls` *and* mirror the call as
/// text in `delta.content`, wrapped in sentinel tokens. That mirror is not
/// assistant prose. Feeding each content delta through [`feed`](Self::feed)
/// suppresses it before it ever becomes a `TextDelta`, so the UI never
/// flickers and the harness needs no after-the-fact retraction.
///
/// Content is treated as an echo when it contains a sentinel token, or when it
/// is nothing but JSON object(s) carrying a `tool`/`name` key (with optional
/// surrounding whitespace). Everything else passes through unchanged; sentinel
/// tokens split across deltas are still recognised.
struct ToolCallEchoFilter {
    /// Unclassified text: may still be the prefix of a sentinel token or an
    /// incomplete JSON object.
    pending: String,
    /// Text classified as a tool-call echo, held until the stream ends. Whether
    /// it is dropped depends on whether native `tool_calls` also arrived: with
    /// them it is a redundant mirror (drop); without them it is a real
    /// text-emitted tool call the harness must still parse (emit).
    held: String,
    /// In hold mode: every subsequent delta appends to `held`.
    echo: bool,
    /// Set when the stream produced at least one native `ToolCallDelta` — the
    /// decision input for [`ToolCallEchoFilter::finish`].
    had_native_tool_calls: bool,
    /// Diagnostics accumulated across the stream: chars fed vs emitted (their
    /// difference is what the filter suppressed), plus reasoning/tool-call
    /// traffic. Logged once at stream end so an "empty assistant response" can
    /// be traced to its cause (genuine empty vs filter suppression vs parser).
    fed_chars: usize,
    emitted_chars: usize,
    reasoning_chars: usize,
    tool_call_deltas: usize,
}

/// Maximum bytes of `{`-prefixed content to buffer while deciding whether it is
/// a tool-call echo. Real tool calls are far smaller; exceeding this flushes
/// the buffer as ordinary text so a large legitimate JSON response is not held
/// back indefinitely.
const MAX_ECHO_BUFFER: usize = 8192;

impl ToolCallEchoFilter {
    fn new() -> Self {
        Self {
            pending: String::new(),
            held: String::new(),
            echo: false,
            had_native_tool_calls: false,
            fed_chars: 0,
            emitted_chars: 0,
            reasoning_chars: 0,
            tool_call_deltas: 0,
        }
    }

    /// Feed a content delta; returns the text safe to emit now. Tool-call-shaped
    /// content is *held* (not dropped) until [`finish`](Self::finish) resolves
    /// it against whether native tool calls arrived.
    fn feed(&mut self, delta: &str) -> String {
        self.fed_chars += delta.len();
        if self.echo {
            self.held.push_str(delta);
            return String::new();
        }
        self.pending.push_str(delta);

        // A sentinel token anywhere means the content is a tool-call section —
        // hold it for the stream-end decision.
        if TOOL_CALL_SENTINELS
            .iter()
            .any(|token| self.pending.contains(token))
        {
            self.enter_hold();
            return String::new();
        }

        let trimmed = self.pending.trim_start();
        if trimmed.starts_with('{') {
            let brace = self.pending.len() - trimmed.len();
            return self.classify_json_prefix(brace);
        }

        // Ordinary prose: emit everything that cannot be the start of a
        // sentinel token, holding a short ASCII tail back so a sentinel split
        // across two deltas is still recognised on the next call.
        let emit = prose_emit_len(&self.pending);
        if emit > 0 {
            let out = self.pending[..emit].to_string();
            self.pending = self.pending[emit..].to_string();
            self.emitted_chars += out.len();
            return out;
        }
        String::new()
    }

    /// Move `pending` into `held` and enter hold mode.
    fn enter_hold(&mut self) {
        self.held.push_str(&self.pending);
        self.pending.clear();
        self.echo = true;
    }

    /// Flush whatever remains once the stream ends. Held echo text is dropped
    /// only when native tool calls were also produced (it was a redundant
    /// mirror); otherwise it is emitted so the harness can parse a text
    /// tool-call fallback.
    fn finish(&mut self) -> String {
        if self.echo {
            if self.had_native_tool_calls {
                self.held.clear();
                return String::new();
            }
            let out = std::mem::take(&mut self.held);
            self.emitted_chars += out.len();
            return out;
        }
        let out = std::mem::take(&mut self.pending);
        self.emitted_chars += out.len();
        out
    }

    /// `self.pending[brace..]` begins with `{`. If the object is complete,
    /// classify it; otherwise keep buffering (or flush if it has grown too
    /// large to plausibly be a tool call).
    fn classify_json_prefix(&mut self, brace: usize) -> String {
        match crate::tool_call::find_balanced_json_object(&self.pending, brace) {
            Some(end) => {
                let candidate = &self.pending[brace..=end];
                let is_tool_call = serde_json::from_str::<Value>(candidate)
                    .map(|value| {
                        value
                            .get("tool")
                            .or_else(|| value.get("name"))
                            .and_then(|node| node.as_str())
                            .is_some()
                    })
                    .unwrap_or(false);
                if is_tool_call {
                    // Hold everything; the stream-end decision resolves mirror
                    // vs real text tool call.
                    self.enter_hold();
                    String::new()
                } else {
                    // Valid JSON but not a tool call — ordinary content.
                    let out = std::mem::take(&mut self.pending);
                    self.emitted_chars += out.len();
                    out
                }
            }
            None => {
                if self.pending.len() > MAX_ECHO_BUFFER {
                    let out = std::mem::take(&mut self.pending);
                    self.emitted_chars += out.len();
                    out
                } else {
                    String::new()
                }
            }
        }
    }
}

/// Largest prefix length of `pending` that is safe to emit now, retaining any
/// trailing suffix that could be the start of a sentinel token.
fn prose_emit_len(pending: &str) -> usize {
    let max_sentinel = TOOL_CALL_SENTINELS
        .iter()
        .map(|token| token.len())
        .max()
        .unwrap_or(0);
    let scan_from = pending.len().saturating_sub(max_sentinel);
    let bytes = pending.as_bytes();
    let mut cursor = scan_from;
    while cursor < bytes.len() {
        if pending.is_char_boundary(cursor) {
            let suffix = &pending[cursor..];
            if TOOL_CALL_SENTINELS
                .iter()
                .any(|token| token.starts_with(suffix))
            {
                return cursor;
            }
        }
        cursor += 1;
    }
    bytes.len()
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
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
        let reasoning_content = choice["reasoning_content"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        let tool_calls: Option<Vec<ToolCall>> = choice.get("tool_calls").and_then(|tc| {
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

        // Strip a tool-call mirror from `content` only when native `tool_calls`
        // are also present; otherwise the text is a real fallback tool call (or
        // ordinary prose) the harness must see.
        let content = {
            let mut filter = ToolCallEchoFilter::new();
            let _ = filter.feed(choice["content"].as_str().unwrap_or(""));
            filter.had_native_tool_calls =
                tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
            let content = filter.finish();
            tracing::debug!(
                target: "neenee_core::provider",
                provider = %self.id,
                model = %self.model,
                content_fed_chars = filter.fed_chars,
                content_emitted_chars = filter.emitted_chars,
                echo_suppressed_chars = filter.fed_chars.saturating_sub(filter.emitted_chars),
                native_tool_calls = filter.had_native_tool_calls,
                "openai chat summary",
            );
            content
        };

        Ok(Message {
            role: Role::Assistant,
            content,
            content_blob: None,
            display_content: None,
            reasoning_content,
            tool_calls,
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
        // Tool-call echo filter shared between the body and the end-of-stream
        // flush: it suppresses any content that mirrors a native tool call
        // (see `ToolCallEchoFilter`) before it becomes a `TextDelta`.
        let echo_filter = Arc::new(Mutex::new(ToolCallEchoFilter::new()));
        let filter_for_body = Arc::clone(&echo_filter);
        let body = response.bytes_stream().map(move |item| {
            let bytes = item.map_err(|error| transport_error("OpenAI", error))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            let mut parsed = Vec::new();
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer.drain(..pos + 1);
                if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
                    if data != "[DONE]" {
                        parsed.extend(parse_openai_stream_data(data));
                    }
                }
            }
            let mut filter = filter_for_body
                .lock()
                .expect("tool-call echo filter mutex poisoned");
            let mut events: Vec<Result<ProviderStreamEvent, String>> = Vec::new();
            for event in parsed {
                match event {
                    ProviderStreamEvent::TextDelta(text) => {
                        let emitted = filter.feed(&text);
                        if !emitted.is_empty() {
                            events.push(Ok(ProviderStreamEvent::TextDelta(emitted)));
                        }
                    }
                    ProviderStreamEvent::ReasoningDelta(delta) => {
                        filter.reasoning_chars += delta.len();
                        events.push(Ok(ProviderStreamEvent::ReasoningDelta(delta)));
                    }
                    ProviderStreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    } => {
                        filter.tool_call_deltas += 1;
                        filter.had_native_tool_calls = true;
                        events.push(Ok(ProviderStreamEvent::ToolCallDelta {
                            index,
                            id,
                            name,
                            arguments,
                        }));
                    }
                }
            }
            Ok::<_, String>(events)
        });
        // Flush any buffered non-echo text once the byte stream ends, and log a
        // per-turn stream summary so empty responses are diagnosable.
        let provider_id = self.id.clone();
        let model = self.model.clone();
        let tail = futures::stream::once(async move {
            let mut filter = echo_filter
                .lock()
                .expect("tool-call echo filter mutex poisoned");
            let emitted = filter.finish();
            tracing::debug!(
                target: "neenee_core::provider",
                provider = %provider_id,
                model = %model,
                content_fed_chars = filter.fed_chars,
                content_emitted_chars = filter.emitted_chars,
                echo_suppressed_chars = filter.fed_chars.saturating_sub(filter.emitted_chars),
                reasoning_chars = filter.reasoning_chars,
                tool_call_deltas = filter.tool_call_deltas,
                "openai stream summary",
            );
            let events: Vec<Result<ProviderStreamEvent, String>> = if emitted.is_empty() {
                Vec::new()
            } else {
                vec![Ok(ProviderStreamEvent::TextDelta(emitted))]
            };
            Ok::<_, String>(events)
        });
        Ok(body
            .chain(tail)
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
/// Every provider in [`OPENAI_PROVIDER_SPECS`] speaks the OpenAI
/// chat-completions wire format and differs only in endpoint, default model,
/// the environment variables consulted, and (rarely) a pinned model or a
/// required user agent. Modelling them as *data* rather than one delegating
/// newtype per vendor means adding a provider is a single table entry instead
/// of ~30 lines of boilerplate trait delegation.
pub struct OpenAiProviderSpec {
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
pub const OPENAI_PROVIDER_SPECS: &[OpenAiProviderSpec] = &[
    // Kimi Code — OpenAI-compatible coding-agent endpoint. The fixed model ID
    // is mapped to the latest coding model by Kimi Code.
    OpenAiProviderSpec {
        id: "kimi-code",
        base_url: "https://api.kimi.com/coding/v1/chat/completions",
        default_model: "kimi-code",
        env_api_key: "KIMI_CODE_API_KEY",
        env_model: "KIMI_CODE_MODEL",
        fixed_model: Some("kimi-code"),
        default_user_agent: Some(KIMI_CODE_USER_AGENT),
    },
    // DeepSeek Flash — fast V3 chat model.
    OpenAiProviderSpec {
        id: "deepseek-flash",
        base_url: "https://api.deepseek.com/v1/chat/completions",
        default_model: "deepseek-chat",
        env_api_key: "DEEPSEEK_API_KEY",
        env_model: "DEEPSEEK_FLASH_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // DeepSeek Pro — R1 reasoning model (returns reasoning_content).
    OpenAiProviderSpec {
        id: "deepseek-pro",
        base_url: "https://api.deepseek.com/v1/chat/completions",
        default_model: "deepseek-reasoner",
        env_api_key: "DEEPSEEK_API_KEY",
        env_model: "DEEPSEEK_PRO_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // Qwen (Tongyi / Alibaba DashScope). Models: qwen-plus, qwen-max, qwen-coder-plus.
    OpenAiProviderSpec {
        id: "qwen",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        default_model: "qwen-plus",
        env_api_key: "DASHSCOPE_API_KEY",
        env_model: "QWEN_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // GLM (Zhipu AI / 智谱). Models: glm-4-plus, glm-4, glm-4-air, glm-4-flash.
    OpenAiProviderSpec {
        id: "glm",
        base_url: "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        default_model: "glm-4-plus",
        env_api_key: "GLM_API_KEY",
        env_model: "GLM_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
];

/// Look up an OpenAI-compatible provider spec by its identifier.
///
/// The legacy `"deepseek"` id transparently resolves to `"deepseek-flash"` so
/// existing configs keep working after the split into Flash / Pro presets.
pub fn openai_provider_spec(id: &str) -> Option<&'static OpenAiProviderSpec> {
    let id = match id {
        "deepseek" => "deepseek-flash",
        other => other,
    };
    OPENAI_PROVIDER_SPECS.iter().find(|spec| spec.id == id)
}

impl OpenAiProviderSpec {
    /// Resolve the model to use: a pinned `fixed_model` always wins, otherwise
    /// the caller's override, otherwise the provider default.
    pub fn resolve_model(&self, override_model: Option<String>) -> String {
        if let Some(fixed) = self.fixed_model {
            return fixed.to_string();
        }
        override_model.unwrap_or_else(|| self.default_model.to_string())
    }

    /// Build a concrete [`OpenAiCompatProvider`] for this spec. `user_agent` overrides
    /// the spec default (used by the Kimi coding endpoint).
    pub fn build(
        &self,
        api_key: String,
        override_model: Option<String>,
        user_agent: Option<String>,
    ) -> OpenAiCompatProvider {
        let model = self.resolve_model(override_model);
        let agent = user_agent
            .or_else(|| self.default_user_agent.map(str::to_string))
            .unwrap_or_else(|| NEENEE_USER_AGENT.to_string());
        let mut provider = OpenAiCompatProvider::with_base_url_and_user_agent(
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

    /// Drive a sequence of content deltas through an echo filter and return
    /// `(surviving_text, echo_flag)` — mirroring how `stream_chat_events`
    /// feeds deltas and then resolves at stream end given whether native
    /// `tool_calls` also arrived.
    fn run_echo_filter(deltas: &[&str], native_tool_calls: bool) -> (String, bool) {
        let mut filter = ToolCallEchoFilter::new();
        let mut out = String::new();
        for delta in deltas {
            out.push_str(&filter.feed(delta));
        }
        filter.had_native_tool_calls = native_tool_calls;
        out.push_str(&filter.finish());
        (out, filter.echo)
    }

    #[test]
    fn echo_filter_drops_mirror_when_native_tool_calls_present() {
        // The GLM leak: a tool call mirrored as text alongside a native call.
        // The mirror is redundant — drop it so raw JSON never reaches the UI.
        let (out, echo) = run_echo_filter(
            &[
                "{\"tool\":\"bash\",\"arguments\":{\"command\":\"git show 493588a\"}}",
                "<|tool_calls_section_end|>",
            ],
            true,
        );
        assert!(echo, "should be classified as an echo");
        assert!(out.is_empty(), "mirror must be dropped: got {out:?}");
    }

    #[test]
    fn echo_filter_drops_multi_argument_tool_call_mirror() {
        // edit_file with special chars (colons, hyphens) inside string values.
        let (out, echo) = run_echo_filter(
            &[
                "{\"tool\":\"edit_file\",\"arguments\":{\"path\":\"docs/adr/0001-tool-rendering-redesign.md\",\"old_string\":\"- Status: Accepted\",\"new_string\":\"- Status: Implemented\"}}",
                "<|tool_calls_section_end|>",
            ],
            true,
        );
        assert!(echo);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn echo_filter_drops_bare_json_mirror_when_native_calls_present() {
        let (out, echo) = run_echo_filter(&["{\"name\":\"read_file\",\"arguments\":{}}"], true);
        assert!(echo);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn echo_filter_buffers_until_json_completes_no_flicker() {
        // The JSON arrives in fragments. Until it is complete and classified,
        // nothing is emitted — so the body never flickers on screen.
        let (out, echo) = run_echo_filter(&["{\"too", "l\":\"bash\",\"arguments\":{}}"], true);
        assert!(echo);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn echo_filter_recognises_sentinel_split_across_deltas() {
        let (out, echo) = run_echo_filter(
            &[
                "<|tool_calls_secti",
                "on_end|>",
                "{\"tool\":\"bash\",\"arguments\":{}}",
            ],
            true,
        );
        assert!(echo);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn echo_filter_restores_text_fallback_when_no_native_calls() {
        // A provider that emits the tool call ONLY as text (no native
        // function calling) must NOT have it stripped — the harness parses it
        // via the text-fallback path. This is the empty-response guard.
        let (out, echo) = run_echo_filter(
            &["{\"tool\":\"bash\",\"arguments\":{\"command\":\"ls\"}}<|tool_calls_section_end|>"],
            false,
        );
        assert!(echo, "still classified as tool-call-shaped");
        assert!(
            !out.is_empty() && out.contains("\"tool\":\"bash\""),
            "text tool call must be restored when no native calls: got {out:?}"
        );
    }

    #[test]
    fn echo_filter_passes_through_plain_prose() {
        let (out, echo) = run_echo_filter(&["Let me read that file ", "for you."], false);
        assert!(!echo);
        assert_eq!(out, "Let me read that file for you.");
    }

    #[test]
    fn echo_filter_keeps_prose_with_embedded_non_tool_json() {
        let (out, echo) = run_echo_filter(&["Here is data: {\"key\":42} done"], false);
        assert!(!echo);
        assert_eq!(out, "Here is data: {\"key\":42} done");
    }

    #[test]
    fn echo_filter_holds_everything_once_a_tool_call_is_seen() {
        // A tool-call object followed by more text is held as a unit and
        // resolved at stream end: dropped with native calls, restored without.
        let (out, echo) = run_echo_filter(
            &["{\"tool\":\"bash\",\"arguments\":{}} now running it"],
            true,
        );
        assert!(echo);
        assert!(
            out.is_empty(),
            "held content is dropped when native calls arrive: got {out:?}"
        );

        let (out, echo) = run_echo_filter(
            &["{\"tool\":\"bash\",\"arguments\":{}} now running it"],
            false,
        );
        assert!(echo);
        assert_eq!(out, "{\"tool\":\"bash\",\"arguments\":{}} now running it");
    }

    #[test]
    fn openai_request_filters_empty_assistant_history() {
        let provider = OpenAiCompatProvider::new("test-key".to_string(), "test-model".to_string());
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
        let provider = OpenAiCompatProvider::new("test-key".to_string(), "test-model".to_string());
        let matched = ToolCall {
            id: "call_matched".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        };
        let assistant_with_call = Message {
            role: Role::Assistant,
            content: String::new(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            tool_calls: Some(vec![matched.clone()]),
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            subagent_meta: None,
        };
        let good_result = Message {
            role: Role::Tool,
            content: "ok".to_string(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some("call_matched".to_string()),
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            subagent_meta: None,
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
        let spec = openai_provider_spec("kimi-code").expect("kimi-code spec");
        // A pinned model ignores any caller override.
        assert_eq!(spec.resolve_model(Some("ignored".to_string())), "kimi-code");

        let provider = spec.build("test-key".to_string(), None, None);
        assert_eq!(provider.base_url, spec.base_url);
        assert_eq!(provider.model, "kimi-code");
        assert_eq!(provider.user_agent, KIMI_CODE_USER_AGENT);
        // The registry stamps the preset id onto the concrete provider so
        // assistant responses can be attributed to "kimi-code".
        assert_eq!(provider.id, "kimi-code");
        assert_eq!(provider.provider_id(), "kimi-code");
        assert_eq!(provider.model(), "kimi-code");
    }

    #[test]
    fn openai_compat_spec_resolves_model_override_and_default() {
        let spec = openai_provider_spec("deepseek-flash").expect("deepseek-flash spec");
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
    fn deepseek_pro_defaults_to_reasoner() {
        let spec = openai_provider_spec("deepseek-pro").expect("deepseek-pro spec");
        assert_eq!(spec.resolve_model(None), "deepseek-reasoner");
        assert_eq!(
            spec.base_url,
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn legacy_deepseek_id_resolves_to_flash() {
        // A pre-split config with default_provider = "deepseek" must keep
        // resolving instead of silently falling back to the mock provider.
        let spec = openai_provider_spec("deepseek").expect("legacy deepseek alias");
        assert_eq!(spec.id, "deepseek-flash");
        assert_eq!(spec.default_model, "deepseek-chat");
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
