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
//! [`Message`]. Tool-call argument JSON is accumulated
//! from `input_json_delta` fragments the same way the OpenAI provider
//! accumulates `tool_calls[].function.arguments`.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use neenee_core::{Message, Provider, ProviderStreamEvent, Role, TokenUsage, Tool, ToolCall};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;

use crate::{NEENEE_USER_AGENT, ensure_success, transport_error};

/// The `anthropic-version` header pinned for the Messages API. opencode-go's
/// `/v1/messages` surface accepts this value; it is the canonical stable
/// version advertised by Anthropic-compatible relays.
const ANTHROPIC_VERSION: &str = "2023-06-01";

// `Effort` and its capability sets (`EFFORT_CLAUDE_FULL`, `EFFORT_COMMON`)
// live in `neenee-core` (`neenee_core::effort`) because effort is a *model
// capability*, not an Anthropic transport detail: which levels a model honors
// is recorded on `neenee_core::Model::effort_levels`. Re-exported here so
// callers that reach the type through the provider crate keep a stable path.
pub use neenee_core::effort::Effort;

/// Whether extended thinking is requested and how. Re-exported from
/// `neenee-core` (its canonical home — thinking on/off is a *model
/// capability*, not a transport detail, exactly like [`Effort`]).
///
/// `Adaptive` is the only supported on-mode for every current Anthropic model
/// that still accepts a `thinking` object at all (Fable/Opus-4.7+ reject the
/// legacy `enabled`+`budget_tokens` form with 400). `Off` omits the field
/// entirely, which disables thinking on models where that is honored
/// (Opus-4.7/4.8); on Fable/Mythos thinking is always on and cannot be
/// disabled, so `Off` is a no-op there.
pub use neenee_core::ThinkingMode;

/// Resolved thinking/effort configuration for an Anthropic Messages provider.
///
/// Carried on the provider so every request stamps the same knobs without the
/// caller threading them through each `chat` call. The two knobs are
/// **orthogonal** ([`ThinkingMode`] = on/off switch, [`Effort`] = depth
/// throttle) and are surfaced as such — never coupled.
///
/// **Reasoning is opt-in.** The default for *every* model is thinking **off**
/// with no explicit effort — extended thinking is a per-model decision the user
/// makes in the model editor, not something a model enables on its own. A
/// request only carries a `thinking` object when the user has turned it on for
/// that model (ADR-0046).
///
/// `effort` is `Option<Effort>`:
/// - `None` — no explicit choice: use the model default (`high`) and **omit**
///   `output_config` from the wire (keeps requests lean, and avoids sending
///   `output_config` to relays that may not expect it).
/// - `Some(e)` — an explicit user override: always emit
///   `output_config: {effort: e}`, **even when `e == High`**. This is the
///   crucial correctness fix: previously an explicit `effort = "high"` was
///   silently dropped (indistinguishable from "no choice"), so a config that
///   pinned `high` was a no-op. An explicit choice must be honored verbatim —
///   "what you set is what you send".
///
/// The chosen effort is clamped to the model's [`effort_levels`] at
/// request-build time.
///
/// [`effort_levels`]: neenee_core::model::Model::effort_levels
#[derive(Debug, Clone, Copy)]
pub struct ThinkingConfig {
    pub mode: ThinkingMode,
    pub effort: Option<Effort>,
}

impl ThinkingConfig {
    /// The default configuration for a model: **thinking off, no explicit
    /// effort**. Extended thinking is opt-in (ADR-0046) — the model editor is
    /// the single place a user turns it on and picks an effort, and a model
    /// with no per-model setting never reasons on its own. This holds whether
    /// or not the model declares `effort_levels`: the *capability set* (which
    /// levels a model honors once thinking is on) is still model-derived, but
    /// *whether* it thinks is the user's choice, not the model's default.
    pub fn for_model(_model: &neenee_core::Model) -> Self {
        Self::default()
    }

    /// Default: thinking off, no explicit effort (model default `high`,
    /// `output_config` omitted).
    pub const fn default() -> Self {
        Self {
            mode: ThinkingMode::Off,
            effort: None,
        }
    }

    /// Set the thinking mode. Returns `self` for chaining.
    pub fn with_mode(mut self, mode: ThinkingMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set an explicit effort override. The value is **not** clamped here;
    /// clamping against the model's supported levels happens at request-build
    /// time so a config assembled before the model is known still resolves
    /// correctly. Once set (even to [`Effort::High`]) the request will emit
    /// `output_config` — see the struct docs. Returns `self` for chaining.
    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Resolve this config against a concrete model's `effort_levels`,
    /// returning a new config whose explicit effort (if any) is clamped to the
    /// model's supported levels (so an `xhigh` request on a model that tops
    /// out at `high` becomes `high`, never an unsupported value). The mode and
    /// the effort's explicit/implicit distinction are honored unchanged. An
    /// empty `effort_levels` disables clamping. This is what `request_body`
    /// calls.
    fn resolve_for(self, effort_levels: &[Effort]) -> Self {
        if effort_levels.is_empty() {
            return self;
        }
        Self {
            mode: self.mode,
            effort: self.effort.map(|e| e.clamp_to(effort_levels)),
        }
    }
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        // const default would conflict with the `Default` derive story used
        // elsewhere; keep one source of truth.
        Self {
            mode: ThinkingMode::Off,
            effort: None,
        }
    }
}

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
    /// Resolved thinking/effort knobs stamped onto every request body. Defaults
    /// to thinking-off and is set by the catalog (per model family) or a caller
    /// via [`Self::with_thinking`].
    pub thinking: ThinkingConfig,
    tools: Mutex<Option<Vec<Value>>>,
    /// Stash for the `usage` object returned by the most recent request, drained
    /// by [`Provider::take_last_usage`]. Populated by both the streaming
    /// (`message_delta.usage`) and non-streaming (`response.usage`) paths.
    last_usage: Mutex<Option<TokenUsage>>,
    /// Stash for the `signature` of the most recent assistant `thinking` block.
    /// Extended thinking signs each block so the server can reconstruct it on
    /// multi-turn replay; we collect it here (streaming accumulates it across
    /// `signature_delta` events, non-streaming reads it once off the block) and
    /// tuck it into the returned [`Message`]'s `provider_meta` under the
    /// `"thinking_signature"` key. [`anthropic_message`] reads that key back to
    /// re-emit a signed thinking block on the next turn. This stays
    /// provider-internal: `core`/`agent` never inspect it.
    ///
    /// `Arc<Mutex<..>>` (not bare `Mutex`) so the streaming path can clone a
    /// `'static` handle into the `BoxStream` closure and accumulate signature
    /// fragments across SSE chunks without borrowing `&self` (which the
    /// `'static` stream lifetime forbids).
    last_thinking_signature: Arc<Mutex<Option<String>>>,
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
        // Default the thinking/effort config to opt-in off (ADR-0046): no
        // model reasons unless the user has explicitly turned it on per-model.
        let thinking = ThinkingConfig::for_model(&neenee_core::model::resolve(&model));
        Self {
            api_key,
            model,
            base_url: base_url.to_string(),
            user_agent: user_agent.to_string(),
            id: "anthropic".to_string(),
            max_tokens: 8192,
            thinking,
            tools: Mutex::new(None),
            last_usage: Mutex::new(None),
            last_thinking_signature: Arc::new(Mutex::new(None)),
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

    /// Override the thinking/effort configuration. The catalog sets a sensible
    /// default per model family at construction; this lets a caller (e.g. the
    /// provider picker, surfacing an effort selector) retune it. Effort is
    /// clamped to the model family's supported levels at request-build time, so
    /// an `xhigh` request on a model that tops out at `high` is silently
    /// downgraded rather than rejected upstream.
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = thinking;
        self
    }

    /// Drain and return the signature collected for the most recent assistant
    /// `thinking` block, if any. The caller (the streaming/non-streaming chat
    /// paths) reads this to stamp the returned [`Message`]'s `provider_meta`
    /// before the next request would clobber it.
    fn take_last_thinking_signature(&self) -> Option<String> {
        self.last_thinking_signature
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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
        // Resolve the thinking/effort config against this model's family and
        // stamp it. `adaptive` is the only on-mode for every current model that
        // accepts a `thinking` object at all; the legacy `enabled`+
        // `budget_tokens` form is rejected (400) on Fable/Opus-4.7+, so it is
        // never emitted. Effort lives in the top-level `output_config`, not in
        // `thinking` — placing it wrong is the most common 400 source.
        // Resolve the thinking/effort config against this model's registered
        // `effort_levels` and stamp it. The clamp range comes from the model
        // registry (the single source of truth), not a family guess. A model
        // with empty `effort_levels` skips clamping; combined with the mode
        // check below this means a non-reasoning / non-Anthropic model emits
        // no `thinking` field at all.
        let model_levels = neenee_core::model::resolve(&self.model).effort_levels;
        let resolved = self.thinking.resolve_for(model_levels);
        if let ThinkingMode::Adaptive = resolved.mode {
            // `display:"summarized"` is load-bearing: Opus 4.7/4.8 and Fable 5
            // default to `display:"omitted"` when the field is absent, which
            // streams ONLY `signature_delta` (captured silently into the
            // signature stash) and NO `thinking_delta` — so the transcript would
            // never show the reasoning. `summarized` makes the server stream
            // readable thinking text as `thinking_delta` events, which the
            // pipeline renders via `TranscriptMessage::thinking`. The signature
            // still arrives and is replayed verbatim on the next turn either way.
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
        }
        // `high` is the wire default, so an *implicit* default omits
        // `output_config` entirely (keeps requests lean, and avoids sending
        // `output_config` to relays that may not expect it). An *explicit*
        // choice is honored verbatim — even `high` — so that "what you set is
        // what you send". Previously this compared against `Effort::High` and
        // silently dropped an explicit `effort = "high"`, making the config a
        // no-op; the explicit/implicit distinction (`Option<Effort>`) fixes it.
        if let Some(effort) = resolved.effort {
            body["output_config"] = json!({ "effort": effort.as_str() });
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
            // Replay the prior turn's thinking FIRST (Anthropic requires a
            // thinking block to precede text/tool_use). We echo it back only
            // when we have reasoning text AND its server-assigned signature —
            // the signature is what lets the upstream reconstruct the original
            // reasoning rather than re-derive it, and a thinking block without
            // one triggers `invalid signature in thinking block` (400) on
            // strict upstreams. Reasoning text alone (no signature) is sent
            // unsigned: correct for Anthropic-compatible relays that produce
            // none, and the server tolerates it there.
            if let Some(reasoning) = m.reasoning_content.as_ref()
                && !reasoning.is_empty()
            {
                let signature = thinking_signature_of(&m);
                let mut block = json!({"type":"thinking","thinking": reasoning});
                if let Some(sig) = signature {
                    block["signature"] = json!(sig);
                }
                blocks.push(block);
            }
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

/// Read the persisted thinking-block signature out of a message's
/// `provider_meta` sidecar (the Anthropic provider stashes it there under the
/// `"thinking_signature"` key). Returns `None` when the message carries no
/// signature — e.g. a relay that produced unsigned thinking, or a message from
/// a different provider. Provider-internal: only [`anthropic_message`] calls
/// this, so `core`/`agent` never touch the sidecar key.
fn thinking_signature_of(m: &Message) -> Option<String> {
    m.provider_meta
        .as_ref()?
        .get("thinking_signature")?
        .as_str()
        .map(str::to_string)
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

/// Parse an Anthropic `usage` object (`input_tokens` / `output_tokens`) into a
/// [`TokenUsage`]. Returns `None` when the object is absent or has no numeric
/// fields. `prompt_tokens` ← `input_tokens`, `completion_tokens` ←
/// `output_tokens`, `total_tokens` ← their sum.
fn parse_anthropic_usage(usage: &Value) -> Option<TokenUsage> {
    let input = usage["input_tokens"].as_i64();
    let output = usage["output_tokens"].as_i64();
    let (p, c) = match (input, output) {
        (Some(p), Some(c)) => (p, c),
        (Some(p), None) => (p, 0),
        (None, Some(c)) => (0, c),
        (None, None) => return None,
    };
    Some(TokenUsage {
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: p + c,
    })
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
        // message_delta carries the final cumulative `usage` (input +
        // output tokens) right before `message_stop`. Forward it as a Usage
        // event so the harness books authoritative counts instead of
        // estimating. message_start / content_block_stop / message_stop carry
        // no content to forward; a normal stream end is observed by the harness
        // simply as the byte stream closing.
        "message_delta" => {
            if let Some(usage) = parse_anthropic_usage(&value["usage"]) {
                Ok(vec![ProviderStreamEvent::Usage(usage)])
            } else {
                Ok(Vec::new())
            }
        }
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

    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn usage_supported(&self) -> bool {
        true
    }

    fn take_last_usage(&self) -> Option<TokenUsage> {
        self.last_usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn take_last_provider_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        // Drain the thinking signature accumulated during the last turn
        // (streaming: across `signature_delta` SSE chunks; non-streaming: read
        // once off the `thinking` block) into the provider-opaque sidecar the
        // harness stamps on the assistant message. The private helper already
        // backs the non-streaming `chat()` path; the streaming path relies on
        // this trait method, called by the harness after the stream ends.
        self.take_last_thinking_signature().map(|sig| {
            let mut map = serde_json::Map::new();
            map.insert("thinking_signature".to_string(), Value::String(sig));
            map
        })
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
                        // The thinking block carries a `signature` the server
                        // needs to reconstruct it on the next replay (present
                        // on real Claude upstreams; absent on unsigned relays).
                        // Stash it for the message we return below.
                        if let Some(sig) = block["signature"].as_str()
                            && !sig.is_empty()
                        {
                            *self
                                .last_thinking_signature
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = Some(sig.to_string());
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

        // Parse the top-level `usage` object (input_tokens / output_tokens).
        // Anthropic reports authoritative token counts here; we stash them so
        // the harness can drain them via `take_last_usage` instead of guessing.
        let usage = parse_anthropic_usage(&response_json["usage"]);
        if let Some(usage) = usage {
            *self.last_usage.lock().unwrap_or_else(|e| e.into_inner()) = Some(usage);
        }

        // Drain the thinking signature captured above into the returned
        // message's provider-opaque sidecar so the next replay re-emits a
        // signed thinking block. `None` (no thinking, or an unsigned relay)
        // leaves the sidecar empty, which `anthropic_message` treats as
        // "no signature to replay".
        let thinking_signature = self.take_last_thinking_signature();
        let provider_meta = thinking_signature.map(|sig| {
            let mut map = serde_json::Map::new();
            map.insert("thinking_signature".to_string(), Value::String(sig));
            map
        });

        Ok(Message {
            role: Role::Assistant,
            content,
            content_blob: None,
            display_content: None,
            reasoning_content,
            provider_meta,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            envoy_meta: None,
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
            if let Ok(v) = serde_json::from_str::<Value>(&data)
                && v["type"].as_str() == Some("content_block_delta")
                && v["delta"]["type"].as_str() == Some("text_delta")
                && let Some(t) = v["delta"]["text"].as_str()
            {
                text.push_str(t);
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
        // in-stream `error` event surfaces as an Err item. A `signature_delta`
        // (the encrypted thinking credential, streamed when `display:"omitted"`)
        // is accumulated into the provider's `last_thinking_signature` stash so
        // the assembled assistant turn can carry it in `provider_meta` for the
        // next replay — it never surfaces as a stream event, since it is a
        // wire-protocol detail, not a semantic one. The Arc is cloned into the
        // `'static` stream closure (shared with the provider) so fragments
        // accumulate across SSE chunks; the harness drains the stash once after
        // the stream ends via `take_last_thinking_signature`.
        let sig_stash = self.last_thinking_signature.clone();
        let stream = crate::sse::data_payloads(response, "Anthropic").flat_map(move |item| {
            let events: Vec<Result<ProviderStreamEvent, String>> = match item {
                Ok(d) => {
                    // Side-channel: hoover up signature fragments before the
                    // typed parser (which ignores `signature_delta`).
                    capture_signature_delta(&d, &sig_stash);
                    match parse_anthropic_stream_data(&d) {
                        Ok(parsed) => parsed.into_iter().map(Ok).collect(),
                        Err(e) => vec![Err(e)],
                    }
                }
                Err(e) => vec![Err(e)],
            };
            futures::stream::iter(events)
        });
        Ok(stream.boxed())
    }
}

/// Scan one SSE data payload for an Anthropic `signature_delta` and, if found,
/// append its fragment to `stash` (creating the entry on the first fragment).
/// The signature is built up across one or more `signature_delta` events under
/// a single thinking block; concatenated in arrival order it is the full
/// credential the server needs to reconstruct that block on replay. No-op for
/// any other event type. Provider-internal.
fn capture_signature_delta(data: &str, stash: &Mutex<Option<String>>) {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return;
    };
    if value["type"].as_str() != Some("content_block_delta") {
        return;
    }
    if value["delta"]["type"].as_str() != Some("signature_delta") {
        return;
    }
    if let Some(frag) = value["delta"]["signature"].as_str() {
        let mut guard = stash.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_or_insert_with(String::new).push_str(frag);
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

    // ── extended-thinking replay ────────────────────────────────────────

    #[test]
    fn assistant_message_replays_signed_thinking_block() {
        // A prior assistant turn carried reasoning + its server signature
        // (stashed in provider_meta by the chat path). Re-serializing it must
        // emit a thinking block FIRST with the signature intact, then text.
        let prior = Message {
            role: Role::Assistant,
            content: "answer".to_string(),
            reasoning_content: Some("let me think".to_string()),
            provider_meta: Some({
                let mut m = serde_json::Map::new();
                m.insert(
                    "thinking_signature".to_string(),
                    Value::String("sig_abc".to_string()),
                );
                m
            }),
            ..Message::new(Role::Assistant, "")
        };
        let wire = anthropic_message(prior);
        let blocks = wire["content"]
            .as_array()
            .expect("content is a block array");
        // thinking first, then text.
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "let me think");
        assert_eq!(
            blocks[0]["signature"], "sig_abc",
            "signature must round-trip"
        );
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "answer");
    }

    #[test]
    fn assistant_message_replays_unsigned_thinking_without_signature() {
        // Reasoning text with no signature (e.g. an Anthropic-compatible relay
        // that produces none) is re-emitted as an unsigned thinking block —
        // correct, and tolerated by such relays.
        let prior = Message {
            role: Role::Assistant,
            content: "x".to_string(),
            reasoning_content: Some("hmm".to_string()),
            provider_meta: None,
            ..Message::new(Role::Assistant, "")
        };
        let wire = anthropic_message(prior);
        let block = &wire["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["thinking"], "hmm");
        assert!(
            block.get("signature").is_none(),
            "no signature key when none was captured"
        );
    }

    #[test]
    fn assistant_message_omits_thinking_block_when_no_reasoning() {
        // A plain turn (no reasoning) must not synthesize an empty thinking
        // block — only text + tool_use.
        let prior = Message::new(Role::Assistant, "just text");
        let wire = anthropic_message(prior);
        let blocks = wire["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn claude_request_body_omits_thinking_by_default() {
        // ADR-0046: reasoning is opt-in. A Claude model with no per-model
        // setting sends NO `thinking` object and NO `output_config` — extended
        // thinking is the user's choice (made in the model editor), not the
        // model's default. Previously a Claude model defaulted to adaptive
        // thinking; that is no longer the case.
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://x",
        );
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert!(
            body.get("thinking").is_none(),
            "Claude defaults to thinking off (opt-in)"
        );
        assert!(
            body.get("output_config").is_none(),
            "no explicit effort omits output_config"
        );
    }

    #[test]
    fn claude_request_body_injects_adaptive_thinking_when_opted_in() {
        // When the user DOES turn thinking on for a Claude model, the request
        // body carries `thinking: {type:"adaptive", display:"summarized"}` —
        // `display` is required or Opus 4.7/4.8/Fable 5 stream only the
        // signature (no readable thinking). `high` effort is the wire default,
        // so `output_config` is omitted to keep the request lean.
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://x",
        )
        .with_thinking(
            ThinkingConfig::default()
                .with_mode(ThinkingMode::Adaptive),
        );
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(
            body.get("output_config").is_none(),
            "default high effort omits output_config"
        );
    }

    #[test]
    fn unknown_relay_model_omits_thinking_by_default() {
        // A model id NOT in the known registry resolves to the fallback model,
        // which carries an empty `effort_levels` — so thinking defaults OFF
        // (we don't presume an unknown relay honors the Claude thinking
        // surface). Contrast with a registered Anthropic-compat model like
        // minimax-m3, which IS known to reason and so defaults to adaptive.
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "some-unknown-relay-model".to_string(),
            "https://x",
        );
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert!(
            body.get("thinking").is_none(),
            "unknown relay model defaults to thinking off"
        );
    }

    #[test]
    fn known_relay_model_also_defaults_to_thinking_off() {
        // ADR-0046: the opt-in default is uniform across ALL models, including
        // a registered Anthropic-compat relay model (minimax) that carries
        // EFFORT_COMMON. It is *capable* of reasoning (and supports effort
        // levels once turned on), but thinking is still off until the user
        // opts in — exactly like a first-party Claude model.
        let provider =
            AnthropicMessagesProvider::new("k".to_string(), "minimax-m3".to_string(), "https://x");
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert!(
            body.get("thinking").is_none(),
            "known relay model also defaults to thinking off (opt-in)"
        );
    }

    #[test]
    fn non_default_effort_is_stamped_into_output_config() {
        // An explicitly-set non-high effort lands in top-level output_config
        // (NOT inside thinking — the most common 400 source). Resolve against
        // the real claude model's effort_levels (full range, so Max survives).
        // Thinking is explicitly turned on here (ADR-0046 made it opt-in), so
        // the `thinking` object is present alongside the effort.
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://x",
        )
        .with_thinking(
            ThinkingConfig::default()
                .with_mode(ThinkingMode::Adaptive)
                .with_effort(Effort::Max),
        );
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["thinking"]["type"], "adaptive");
    }

    #[test]
    fn effort_clamps_to_model_support_levels() {
        // A model with the conservative effort set (minimax, EFFORT_COMMON —
        // no xhigh/max) clamps an xhigh request down to high; a model with the
        // full set (claude) honors xhigh.
        let cfg = ThinkingConfig::default().with_effort(Effort::Xhigh);
        let resolved = cfg.resolve_for(neenee_core::EFFORT_COMMON);
        assert_eq!(
            resolved.effort,
            Some(Effort::High),
            "xhigh clamps to high on a common-only model"
        );
        // On a first-party Claude model (full range), xhigh is honored.
        let resolved_claude = cfg.resolve_for(neenee_core::EFFORT_CLAUDE_FULL);
        assert_eq!(resolved_claude.effort, Some(Effort::Xhigh));
    }

    #[test]
    fn explicit_high_effort_is_honored_not_swallowed() {
        // The regression guard: an *explicit* effort of High MUST land in
        // output_config. Before the Option<Effort> rewrite this was silently
        // dropped (a `high` config was indistinguishable from "no choice"), so
        // a channel pinned to `effort = "high"` was a no-op on the wire.
        // (Thinking is orthogonal and off here — ADR-0046 — yet effort still
        // emits, proving the two stay decoupled.)
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://x",
        )
        .with_thinking(ThinkingConfig::default().with_effort(Effort::High));
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert_eq!(
            body["output_config"]["effort"], "high",
            "an explicitly-pinned high effort must be emitted, not swallowed"
        );
    }

    #[test]
    fn effort_without_thinking_stays_decoupled() {
        // effort (depth) and thinking (on/off) are orthogonal on the wire. A
        // request may set effort while keeping thinking OFF — the model just
        // won't reason regardless of depth. This is the decoupling contract:
        // setting effort must NOT implicitly turn thinking on.
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://x",
        )
        .with_thinking(
            ThinkingConfig::default() // mode = Off
                .with_effort(Effort::Medium),
        );
        let body = provider.request_body(vec![Message::new(Role::User, "hi")], false);
        assert_eq!(body["output_config"]["effort"], "medium");
        assert!(
            body.get("thinking").is_none(),
            "effort alone must not enable thinking — the two stay decoupled"
        );
    }

    #[test]
    fn signature_delta_fragments_accumulate_into_stash() {
        // Streaming with display:"omitted" delivers the signature as one or
        // more `signature_delta` events; capture_signature_delta must
        // concatenate them in arrival order.
        let stash = Arc::new(Mutex::new(None));
        capture_signature_delta(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EosnCk"}}"#,
            &stash,
        );
        capture_signature_delta(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"XyZ"}}"#,
            &stash,
        );
        // A non-signature event must not disturb the stash.
        capture_signature_delta(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            &stash,
        );
        let guard = stash.lock().unwrap();
        assert_eq!(guard.as_deref(), Some("EosnCkXyZ"));
    }

    #[test]
    fn take_last_provider_meta_drains_thinking_signature() {
        // The trait method wraps the private stash; draining once leaves None
        // for the next call (consume-once contract).
        let provider = AnthropicMessagesProvider::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://x",
        );
        *provider.last_thinking_signature.lock().unwrap() = Some("sig_xyz".to_string());
        let meta = provider.take_last_provider_meta().expect("some meta");
        assert_eq!(meta["thinking_signature"], "sig_xyz");
        assert!(
            provider.take_last_provider_meta().is_none(),
            "stash drained on first take"
        );
    }
}
