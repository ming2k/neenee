//! Anthropic-compatible `/messages` provider with native tool-call support.
//!
//! Speaks the Anthropic Messages wire protocol used by opencode-go's
//! MiniMax/Qwen models (and any Anthropic-compatible relay). This is the
//! companion of [`OpenAiCompatProvider`](crate::OpenAiCompatProvider): the two
//! cover the wire formats opencode-go hosts, and the catalog picks between them
//! per model via the model's [`WireFormat`](neenee_core::WireFormat).
//!
//! Wire shape:
//! - Auth: `x-api-key: <key>` + `anthropic-version: 2023-06-01`.
//! - Request body: `model`, `messages` (each a `{role, content: [blocks]}`),
//!   `system` (top-level string), `tools` (`[{name, description, input_schema}]`),
//!   `max_tokens`, `stream`.
//! - Content blocks: `{type:"text"|"tool_use"|"tool_result", ...}`.
//! - Streaming: SSE `event:` + `data:` pairs — `message_start`,
//!   `content_block_start` (opens a text/tool_use block by index),
//!   `content_block_delta` (text deltas / `input_json_delta` for tool args /
//!   `thinking_delta` for reasoning), `content_block_stop`, `message_delta`
//!   (stop reason / usage), `message_stop`.
//!
//! Non-streaming chat assembles the same block list into one assistant
//! [`Message`](neenee_core::Message). Tool-call argument JSON is accumulated
//! from `input_json_delta` fragments the same way the OpenAI provider
//! accumulates `tool_calls[].function.arguments`.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use neenee_core::{Message, Provider, ProviderStreamEvent, Role, Tool, ToolCall};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;

use crate::{NEENEE_USER_AGENT, ensure_success, transport_error};

/// The `anthropic-version` header pinned for the Messages API. opencode-go's
/// `/v1/messages` surface accepts this value; it is the canonical stable
/// version advertised by Anthropic-compatible relays.
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicMessagesProvider {
    pub api_key: String,
    pub model: String,
    /// Full `/messages` endpoint URL (e.g.
    /// `https://opencode.ai/zen/go/v1/messages`).
    pub base_url: String,
    pub user_agent: String,
    /// Stable provider/solution id surfaced via [`Provider::provider_id`]. The
    /// catalog stamps the entry id (e.g. `"opencode-go"`) here.
    pub id: String,
    /// `max_tokens` sent on every request. The Messages API requires it; a
    /// generous default keeps long agent turns from truncating.
    pub max_tokens: u32,
    tools: Mutex<Option<Vec<Value>>>,
}

impl AnthropicMessagesProvider {
    pub fn new(api_key: String, model: String, base_url: &str) -> Self {
        Self::with_user_agent(api_key, model, base_url, NEENEE_USER_AGENT)
    }

    pub fn with_user_agent(
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
            id: "anthropic".to_string(),
            max_tokens: 8192,
            tools: Mutex::new(None),
        }
    }

    /// Set the `max_tokens` sent on every `/messages` request. The Messages API
    /// requires this field; it caps the response length. The catalog derives it
    /// from the model's registered output limit (capped to avoid pathological
    /// requests), but a caller may override it (e.g. for a budget-limited relay).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Build the `/messages` request body from the harness message list.
    ///
    /// Anthropic splits `system` out of the message list (it is a top-level
    /// field, not a message role), and every message `content` is an array of
    /// typed blocks. Tool results are `{type:"tool_result"}` blocks on a `user`
    /// message, not a separate `tool` role. The conversion reassembles the
    /// harness's flat `Message` stream into this shape.
    fn request_body(&self, messages: Vec<Message>, stream: bool) -> Value {
        let tools = self.tools.lock().unwrap_or_else(|error| error.into_inner());
        let tool_specs = tools.as_ref().map(|specs| {
            json!(
                specs
                    .iter()
                    .map(|spec| {
                        // The harness produces OpenAI-shaped function specs
                        // ({type:"function", function:{name,description,parameters}}).
                        // Anthropic wants {name, description, input_schema}. The
                        // `parameters` object is already a JSON-Schema fragment
                        // and maps verbatim.
                        let function = &spec["function"];
                        json!({
                            "name": function["name"],
                            "description": function["description"],
                            "input_schema": function.get("parameters")
                                .cloned()
                                .unwrap_or(json!({"type":"object","properties":{}})),
                        })
                    })
                    .collect::<Vec<_>>()
            )
        });

        // Pull leading system message(s) out of the list; Anthropic carries
        // system as a top-level string, not a role.
        let mut system_text = String::new();
        let mut conversation: Vec<Message> = Vec::with_capacity(messages.len());
        for message in messages {
            if message.role == Role::System {
                if !system_text.is_empty() {
                    system_text.push_str("\n\n");
                }
                system_text.push_str(&message.content);
                continue;
            }
            conversation.push(message);
        }

        // Every assistant `tool_calls` must be followed by a corresponding
        // `tool` result.  Collect the ids that got a result, then strip
        // unanswered calls from every assistant message.
        let answered: std::collections::HashSet<String> = conversation
            .iter()
            .filter_map(|m| {
                if m.role == Role::Tool {
                    m.tool_call_id.clone()
                } else {
                    None
                }
            })
            .collect();
        conversation.retain_mut(|m| {
            if m.role != Role::Assistant {
                return true;
            }
            if let Some(calls) = m.tool_calls.as_mut() {
                calls.retain(|c| answered.contains(&c.id));
                if calls.is_empty() {
                    m.tool_calls = None;
                }
            }
            !m.content.is_empty() || m.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
        });

        let mut body = json!({
            "model": self.model,
            "messages": conversation.into_iter().map(anthropic_message).collect::<Vec<_>>(),
            "max_tokens": self.max_tokens,
            "stream": stream,
        });
        if !system_text.is_empty() {
            body["system"] = json!(system_text);
        }
        if let Some(specs) = tool_specs {
            body["tools"] = specs;
        }
        body
    }
}

/// Convert a harness [`Message`] to an Anthropic message object.
///
/// Anthropic roles are `user` and `assistant` only; `tool` results become
/// `user` messages carrying `tool_result` blocks. Content is always a block
/// array; plain text becomes `[{type:"text", text}]`, and images become
/// `image` blocks.
fn anthropic_message(m: Message) -> Value {
    match m.role {
        Role::Tool => {
            // A tool result is a user-role message with a tool_result block.
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.unwrap_or_default(),
                    "content": m.content,
                }],
            })
        }
        Role::Assistant => {
            let mut blocks: Vec<Value> = Vec::new();
            if !m.content.is_empty() {
                blocks.push(json!({"type":"text","text": m.content}));
            }
            if let Some(calls) = m.tool_calls.as_ref() {
                for call in calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": parse_arguments(&call.arguments),
                    }));
                }
            }
            json!({"role": "assistant", "content": blocks})
        }
        _ => {
            // user / system-fallback: content as typed blocks (text + images).
            let blocks = content_blocks(&m);
            json!({"role": "user", "content": blocks})
        }
    }
}

/// Build the Anthropic content block array for a user/system message: a text
/// block for the prose, plus an `image` block per attachment.
fn content_blocks(m: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    if !m.content.is_empty() {
        blocks.push(json!({"type":"text","text": m.content}));
    }
    if let Some(images) = m.images.as_ref() {
        for image in images {
            blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime,
                    "data": image.data,
                },
            }));
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type":"text","text":""}));
    }
    blocks
}

/// Parse a tool-call `arguments` string into a JSON value for the `input`
/// field. The harness stores arguments as a JSON string (possibly empty when
/// the model emitted no arguments); Anthropic requires a JSON object.
fn parse_arguments(arguments: &str) -> Value {
    if arguments.is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(arguments).unwrap_or(json!({}))
}

/// Parse one SSE `data:` payload (already stripped of the `data:` prefix) into
/// provider stream events. Anthropic wraps each event in `{type, ...}`; the
/// `type` discriminator selects the block/delta shape.
///
/// Returns `Err` only for an in-stream `error` event (Anthropic can emit one
/// mid-stream); other non-content events (`message_start`, `content_block_stop`,
/// `message_delta`, `message_stop`) are no-ops that yield no events, so a normal
/// stream end is observed by the harness simply as the byte stream closing.
fn parse_anthropic_stream_data(data: &str) -> Result<Vec<ProviderStreamEvent>, String> {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return Ok(Vec::new());
    };
    let event_type = value["type"].as_str().unwrap_or("");
    match event_type {
        "error" => {
            // Anthropic can emit an error event mid-stream. Surface its message
            // so the harness reports it rather than treating the stream as
            // merely empty.
            let message = value["error"]["message"]
                .as_str()
                .unwrap_or("Anthropic stream error")
                .to_string();
            Err(message)
        }
        "content_block_start" => {
            // Opens a new block at `index`. A tool_use block carries the call
            // id and name up front; its arguments arrive as later deltas.
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            let block = &value["content_block"];
            let block_type = block["type"].as_str().unwrap_or("");
            if block_type == "tool_use" {
                Ok(vec![ProviderStreamEvent::ToolCallDelta {
                    index,
                    id: block["id"].as_str().map(str::to_string),
                    name: block["name"].as_str().map(str::to_string),
                    arguments: String::new(),
                }])
            } else {
                Ok(Vec::new())
            }
        }
        "content_block_delta" => {
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            let delta = &value["delta"];
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => Ok(delta["text"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(|t| ProviderStreamEvent::TextDelta(t.to_string()))
                    .into_iter()
                    .collect()),
                "thinking_delta" => Ok(delta["thinking"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(|t| ProviderStreamEvent::ReasoningDelta(t.to_string()))
                    .into_iter()
                    .collect()),
                "input_json_delta" => {
                    // A fragment of the tool-call argument JSON. Forward as a
                    // tool-call delta; the harness concatenates fragments.
                    let frag = delta["partial_json"].as_str().unwrap_or("");
                    Ok(vec![ProviderStreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments: frag.to_string(),
                    }])
                }
                _ => Ok(Vec::new()),
            }
        }
        // message_start / content_block_stop / message_delta (stop_reason,
        // usage) / message_stop: no content to forward. The harness observes
        // stream end as the byte stream closing.
        _ => Ok(Vec::new()),
    }
}

#[async_trait]
impl Provider for AnthropicMessagesProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) {
        let schemas: Vec<Value> = tools.iter().map(|t| t.to_openai_function()).collect();
        let _ = self.tools.lock().map(|mut guard| {
            *guard = Some(schemas);
        });
    }

    fn prepare_tools_with(
        &self,
        tools: &[Arc<dyn Tool>],
        overrides: &neenee_core::ToolDescriptionOverrides,
    ) {
        let schemas: Vec<Value> = tools
            .iter()
            .map(|t| t.to_openai_function_with(overrides))
            .collect();
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
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("Anthropic", error))?;
        let response = ensure_success(response, "Anthropic").await?;

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;
        if let Some(err) = response_json.get("error") {
            return Err(format!("Anthropic Error: {}", err));
        }

        // Assemble content blocks into a single assistant message.
        let mut content = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        if let Some(blocks) = response_json["content"].as_array() {
            for block in blocks {
                match block["type"].as_str().unwrap_or("") {
                    "text" => {
                        if let Some(text) = block["text"].as_str() {
                            content.push_str(text);
                        }
                    }
                    "thinking" => {
                        if let Some(text) = block["thinking"].as_str() {
                            reasoning_content = Some(text.to_string());
                        }
                    }
                    "tool_use" => {
                        tool_calls.push(ToolCall {
                            id: block["id"]
                                .as_str()
                                .filter(|v| !v.is_empty())
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            arguments: block
                                .get("input")
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
            }
        }

        Ok(Message {
            role: Role::Assistant,
            content,
            content_blob: None,
            display_content: None,
            reasoning_content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            subagent_meta: None,
            origin: None,
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
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("Anthropic", error))?;
        let response = ensure_success(response, "Anthropic").await?;

        // Reuse the shared SSE byte reassembly; each payload is one Anthropic
        // event JSON. Map to text deltas only (this is the simple stream path).
        let stream = crate::sse::data_payloads(response, "Anthropic").map(|item| {
            let data = item?;
            let mut text = String::new();
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if v["type"].as_str() == Some("content_block_delta")
                    && v["delta"]["type"].as_str() == Some("text_delta")
                {
                    if let Some(t) = v["delta"]["text"].as_str() {
                        text.push_str(t);
                    }
                }
            }
            Ok(text)
        });
        Ok(stream.boxed())
    }

    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        let response = reqwest::Client::new()
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&self.request_body(messages, true))
            .send()
            .await
            .map_err(|error| transport_error("Anthropic", error))?;
        let response = ensure_success(response, "Anthropic").await?;

        // Per-block argument accumulators keyed by block index: the harness
        // concatenates `ToolCallDelta.arguments` fragments, so we re-emit the
        // fragments as-is. Text and reasoning deltas pass straight through. An
        // in-stream `error` event surfaces as an Err item.
        let stream = crate::sse::data_payloads(response, "Anthropic").flat_map(|item| {
            let events: Vec<Result<ProviderStreamEvent, String>> = match item {
                Ok(d) => match parse_anthropic_stream_data(&d) {
                    Ok(parsed) => parsed.into_iter().map(Ok).collect(),
                    Err(e) => vec![Err(e)],
                },
                Err(e) => vec![Err(e)],
            };
            futures::stream::iter(events)
        });
        Ok(stream.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_parser_extracts_text_and_tool_deltas() {
        // A text delta event.
        let text_events = parse_anthropic_stream_data(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        )
        .expect("text delta parses");
        assert_eq!(
            text_events,
            vec![ProviderStreamEvent::TextDelta("Hello".to_string())]
        );

        // A tool_use block opening: id and name arrive up front.
        let open_events = parse_anthropic_stream_data(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}"#,
        )
        .expect("content_block_start parses");
        assert_eq!(
            open_events,
            vec![ProviderStreamEvent::ToolCallDelta {
                index: 1,
                id: Some("toolu_1".to_string()),
                name: Some("bash".to_string()),
                arguments: String::new(),
            }]
        );

        // Argument JSON fragments arrive as input_json_delta; the harness
        // concatenates them.
        let frag_events = parse_anthropic_stream_data(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
        )
        .expect("input_json_delta parses");
        assert_eq!(
            frag_events,
            vec![ProviderStreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments: "{\"comm".to_string(),
            }]
        );
    }

    #[test]
    fn stream_parser_extracts_reasoning_deltas() {
        let events = parse_anthropic_stream_data(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
        )
        .expect("thinking_delta parses");
        assert_eq!(
            events,
            vec![ProviderStreamEvent::ReasoningDelta("hm".to_string())]
        );
    }

    #[test]
    fn stream_parser_surfaces_error_events_as_err() {
        // Anthropic can emit an error event mid-stream; the parser must surface
        // it as Err so the harness reports it rather than treating the stream
        // as merely empty.
        let result = parse_anthropic_stream_data(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Overloaded"),
            "error message must be surfaced"
        );
    }

    #[test]
    fn stream_parser_ignores_non_content_events() {
        // message_start / message_delta (stop_reason, usage) / message_stop
        // carry no content to forward; they must parse cleanly to empty.
        for payload in [
            r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"not-json-at-all"#,
        ] {
            let events = parse_anthropic_stream_data(payload).expect("non-content event is ok");
            assert!(
                events.is_empty(),
                "non-content event must yield nothing: {payload}"
            );
        }
    }

    #[test]
    fn request_body_lifts_system_to_top_level() {
        let provider =
            AnthropicMessagesProvider::new("k".to_string(), "minimax-m3".to_string(), "https://x");
        let body = provider.request_body(
            vec![
                Message::new(Role::System, "you are concise"),
                Message::new(Role::User, "hi"),
            ],
            false,
        );
        assert_eq!(body["system"], "you are concise");
        // No system role remains in the message list.
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn request_body_serializes_tool_result_as_user_block() {
        let provider =
            AnthropicMessagesProvider::new("k".to_string(), "minimax-m3".to_string(), "https://x");
        let body = provider.request_body(
            vec![
                Message::new(Role::User, "run it"),
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: Some(vec![ToolCall {
                        id: "toolu_1".to_string(),
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    }]),
                    ..Message::new(Role::Assistant, "")
                },
                Message {
                    role: Role::Tool,
                    content: "done".to_string(),
                    tool_call_id: Some("toolu_1".to_string()),
                    ..Message::new(Role::Tool, "")
                },
            ],
            false,
        );
        let msgs = body["messages"].as_array().unwrap();
        // user, assistant(tool_use), user(tool_result)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn request_body_includes_tools_in_anthropic_shape() {
        let provider =
            AnthropicMessagesProvider::new("k".to_string(), "minimax-m3".to_string(), "https://x");
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(DummyTool)];
        provider.prepare_tools(&tools);
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        let tool = &body["tools"][0];
        assert_eq!(tool["name"], "dummy");
        assert!(tool.get("input_schema").is_some(), "needs input_schema");
        // No OpenAI-shape `function` wrapper leaks through.
        assert!(tool.get("function").is_none());
    }

    struct DummyTool;
    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        async fn call(&self, _args: &str) -> Result<String, String> {
            Ok("ok".to_string())
        }
    }
}
