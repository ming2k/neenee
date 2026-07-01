//! Conversation message types shared across the harness, providers, and UI.

use crate::hooks::HookEventKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

/// Provenance of a message that was inserted by the harness rather than
/// produced by the model or typed by the user. Stamped at every injection
/// site so a persisted transcript can answer "what was injected, where did
/// it come from, and why" — exactly reconstructing the live turn without
/// fragile string-sniffing.
///
/// `origin: None` is the default for every genuine message: real user input,
/// assistant replies, and tool results. Only harness-injected messages carry
/// an origin.
///
/// Kept as `Option<InjectionOrigin>` on [`Message`] with
/// `#[serde(default, skip_serializing_if = "Option::is_none")]` so the wire
/// shape of a default message is unchanged and legacy snapshots / event-log
/// lines load as `origin: None` with no migration (per ADR-0017 / ADR-0022
/// backward-compat contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectionOrigin {
    /// Structured source classifier.
    pub kind: InjectionKind,
    /// Free-form reason — e.g. the hook name, the steering cause, the skill
    /// that fired. `None` when the kind alone is self-describing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl InjectionOrigin {
    pub fn new(kind: InjectionKind) -> Self {
        Self { kind, reason: None }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Closed classifier for every harness injection path. Adding a variant
/// requires stamping it at the corresponding call site; the enum exhaustiveness
/// is the design lever that forces every injection to be traceable.
///
/// Each variant maps 1:1 to a concrete injection site in the harness; the
/// doc-link in each arm is the single source of truth for "where does this
/// come from".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionKind {
    /// A user-configured hook returned `HookOutcome::Inject`. Carries the
    /// lifecycle event so "which hook axis injected this" is recoverable.
    /// Sites: `HookRegistry::{session_start, run_post_tool_use,
    /// run_post_tool_use_failure, check_stop, run_turn}`.
    Hook(HookEventKind),
    /// Pursuit stop-gate re-applied the continuation prompt to keep the model
    /// on-task mid-turn. Site: `PursuitState::inject_continuation` and the
    /// `stop_gate` continuation arm.
    PursuitContinuation,
    /// The active pursuit's objective was changed mid-flight. Site:
    /// `PursuitState::inject_objective_updated`.
    PursuitObjectiveUpdated,
    /// Inter-agent steering note (`AgentOp::InterAgentMessage`, codex
    /// `InterAgentCommunication` analogue). Site: `Agent::drain_inbox`.
    InterAgent,
    /// Visible parent→child steering payload (`AgentOp::InjectUserMessage`,
    /// codex `inject_if_running` analogue). Site: `Agent::drain_inbox`.
    /// Lands as a *visible* user message, hence distinct from `InterAgent`.
    EnvoySteer,
    /// Implicit skill auto-load: the latest user turn mentioned a skill name,
    /// so the skill body was injected in-context. Site:
    /// `Agent::inject_implicit_skills`.
    ImplicitSkill,
    /// System-prompt assembly: the harness rebuilt the head system message
    /// from the live pursuit, tool list, and skills index. Site:
    /// `Agent::{build_system_message, ensure_system_prompt}`.
    SystemPrompt,
    /// Built-in anti-anchoring nudge fired by the deterministic read-loop guard
    /// when the model repeats the same read (a single page or a two-page thrash)
    /// without progress. Detection is pure signature bookkeeping — no model call
    /// — and the nudge is non-terminating: it steers off the loop, the hard
    /// backstops (`hard_stop_turns`, `abort`, `Esc`) still cap. This is a
    /// harness-internal steering injection, distinct from the user-configurable
    /// `Hook(Round)` axis. Site: `Agent::maybe_inject_loop_nudge`
    /// (`crate::loop_guard`).
    LoopReviewNudge,
    /// Context-compaction checkpoint: an LLM summary of archived turns wrapped
    /// under the stable checkpoint header. Site: `checkpoint_message`.
    CompactionCheckpoint,
    /// A harness-internal prompt admitted as a hidden user turn (resume/replay,
    /// envoy tasking, `/review` re-runs). Site: `execute_round`
    /// `input.hidden` branch.
    HiddenTurnInput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Content-addressed storage hash for large payloads. When present the
    /// inline `content` may be empty on disk and is rehydrated from the blob
    /// store on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Provider-opaque sidecar for **wire-protocol detail that does not map to
    /// a cross-provider semantic concept** and therefore has no business as a
    /// named field on this struct. The canonical example is Anthropic's
    /// extended-thinking `signature` — a cryptographic credential the server
    /// requires to reconstruct a prior `thinking` block on multi-turn replay.
    /// It is meaningless to OpenAI/Gemini, so instead of a named
    /// `thinking_signature` field (which would pollute this provider-agnostic
    /// type with one protocol's transport detail), Anthropic-specific values
    /// live under a `"thinking_signature"` key inside this map. Each concrete
    /// provider owns the contract for the keys it reads/writes here; `core`
    /// treats the whole map as an opaque blob that round-trips through
    /// `session.json` (so a resumed session replays thinking correctly) but is
    /// never inspected outside the provider that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_meta: Option<serde_json::Map<String, serde_json::Value>>,
    /// Optional tool calls attached to an assistant message. Marked
    /// `#[serde(default)]` so hand-written or stripped JSON messages (e.g. test
    /// fixtures, externally generated snapshots) can omit the key entirely
    /// instead of having to spell out `"tool_calls": null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Inline image attachments (typically pasted into the prompt). Each part
    /// carries a MIME type and already-base64-encoded bytes so it can be
    /// emitted directly as an OpenAI `image_url` data URL or a Gemini
    /// `inline_data` part. Only user messages normally carry images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImagePart>>,
    /// Identifier of the provider/solution that produced this assistant
    /// message (e.g. `"kimi-code"`, `"gemini"`). Stamped by the harness so a
    /// session that mixes multiple models stays traceable after resume. Other
    /// roles leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model identifier that produced this assistant message (e.g.
    /// `"kimi-code"`). Companion to [`Message::provider`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    /// Nested envoy transcript. Populated only on the `Tool`-role result
    /// message of a `task` tool call (see `EnvoyTool`). Each entry is a
    /// `Message` from the envoy's own conversation (System, User,
    /// Assistant with tool_calls, Tool results, …), in chronological order.
    /// Recursive: an envoy's own `task` results carry their own `children`,
    /// so arbitrarily deep envoy trees round-trip through session.json.
    ///
    /// `None` for every message that is not an envoy's tool result; this
    /// keeps the legacy flat shape unchanged for non-task messages and lets
    /// old session.json files (which predate the field) deserialize as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Message>>,
    /// Metadata about the envoy run that produced [`Message::children`].
    /// Populated only on the same message that has `children = Some(_)`. The
    /// two fields are convention-paired (presence of one implies presence of
    /// the other); they are kept separate rather than bundled into a single
    /// `envoy: Option<Payload>` field so the schema stays backward-
    /// compatible without a custom deserializer — old session.json files
    /// simply have `envoy_meta = None` and `children = Some(...)`, and the
    /// harness fills in best-effort defaults on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envoy_meta: Option<EnvoyMeta>,
    /// Provenance of a harness-injected message (`None` for genuine user input,
    /// assistant replies, and tool results). See [`InjectionOrigin`] / the
    /// closed [`InjectionKind`] classifier. `#[serde(default,
    /// skip_serializing_if = "Option::is_none")]` keeps the wire shape of a
    /// default message unchanged so legacy snapshots and event-log lines load
    /// as `origin: None` without migration (ADR-0017 / ADR-0022).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<InjectionOrigin>,
}

/// Sidecar metadata for an envoy run. Lives next to
/// [`Message::children`] on the same `Tool`-role result message. Captures
/// information that the live event stream knows but the bare transcript
/// cannot reconstruct on resume.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvoyMeta {
    /// The task description supplied by the parent agent (from the `task`
    /// tool_call's `arguments.description` field). Cached here so the TUI
    /// does not have to re-parse the JSON arguments to label the envoy
    /// view's navigation bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Wall-clock duration of the envoy run in milliseconds. Filled from
    /// the parent `record_tool_result`'s `duration_ms` parameter (which
    /// already measures the full envoy run because the `task` tool blocks
    /// until the envoy finishes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Number of read-only tools the envoy had access to. Useful as a
    /// debugging signal when reviewing archived runs.
    #[serde(default)]
    pub toolset_count: u32,
    /// Provider / model that served the envoy. Currently always equal to
    /// the parent's provider/model (EnvoyTool clones the parent's provider),
    /// but persisted separately so a future "cheaper model for envoys"
    /// feature does not require a schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether the envoy finished by hitting an error path (32-round
    /// limit, repeated-call guard, provider error). Mirrors
    /// `ToolOutput::Envoy { summary.starts_with("Error") }` but stored
    /// explicitly so consumers do not have to string-sniff.
    #[serde(default)]
    pub failed: bool,
}

/// An inline image attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImagePart {
    /// MIME type, e.g. `"image/png"`.
    pub mime: String,
    /// Base64-encoded image bytes.
    pub data: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            provider_meta: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            envoy_meta: None,
            origin: None,
        }
    }

    pub fn hidden(role: Role, content: impl Into<String>) -> Self {
        let mut message = Self::new(role, content);
        message.hidden = true;
        message
    }

    /// Construct a hidden user/system message with an explicit injection
    /// origin. This is the canonical constructor for every harness injection
    /// site — it stamps provenance at construction so it can never drift from
    /// the content. Use [`Message::with_origin`] to stamp an existing message.
    pub fn injected(role: Role, content: impl Into<String>, origin: InjectionOrigin) -> Self {
        let mut message = Self::hidden(role, content);
        message.origin = Some(origin);
        message
    }

    /// Stamp / overwrite the injection origin on this message. Builder-style
    /// companion to [`Message::injected`] for sites that build a message via
    /// another constructor first (e.g. `Message::hidden(...).with_origin(...)`).
    pub fn with_origin(mut self, origin: InjectionOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn with_display_content(mut self, content: impl Into<String>) -> Self {
        self.display_content = Some(content.into());
        self
    }

    pub fn with_images(mut self, images: Vec<ImagePart>) -> Self {
        self.images = if images.is_empty() {
            None
        } else {
            Some(images)
        };
        self
    }

    /// Stamp the provider/solution id and model that produced this message,
    /// so the transcript stays traceable when a session spans multiple models.
    pub fn with_attribution(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            provider_meta: None,
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            envoy_meta: None,
            origin: None,
        }
    }

    /// Attach an envoy's full internal transcript to a `Tool`-role result
    /// message. Builder-style companion to [`Message::tool_result`]. Storing
    /// the nested transcript on the result message (rather than on the
    /// assistant `tool_calls` message) keeps the data close to where it was
    /// produced and lets resume reconstruct the envoy view by reading a
    /// single message.
    pub fn with_children(mut self, children: Vec<Message>) -> Self {
        self.children = if children.is_empty() {
            None
        } else {
            Some(children)
        };
        self
    }

    /// Attach envoy sidecar metadata to a `Tool`-role result message.
    /// Pair with [`Message::with_children`]; the two fields travel together
    /// but are kept separate for schema-backward-compat (see
    /// [`Message::envoy_meta`] docs).
    pub fn with_envoy_meta(mut self, meta: EnvoyMeta) -> Self {
        self.envoy_meta = Some(meta);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_without_children_omits_field_in_json() {
        // Legacy compatibility: a normal Message must still serialise without
        // a `children` key so old consumers / tests that match the literal
        // JSON keep working.
        let m = Message::new(Role::User, "hi");
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("children"),
            "json should omit children: {json}"
        );
    }

    #[test]
    fn legacy_json_without_children_deserialises_to_none() {
        // Pre-Phase-3 snapshots must load unchanged.
        let json = r#"{"role":"User","content":"hi","hidden":false}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.content, "hi");
        assert!(m.children.is_none());
    }

    #[test]
    fn children_round_trip_through_json() {
        // A tool result with an envoy transcript must survive a
        // serialise → deserialise round trip with the nested messages intact,
        // including their own nested children (sub-envoys).
        let call = ToolCall {
            id: "call_root".to_string(),
            name: "envoy".to_string(),
            arguments: "{}".to_string(),
        };
        let nested_call = ToolCall {
            id: "call_inner".to_string(),
            name: "grep".to_string(),
            arguments: r#"{"pattern":"foo"}"#.to_string(),
        };
        let inner_child = Message::new(Role::Tool, "match at a.rs:1")
            .with_children(vec![Message::new(Role::Assistant, "deeply nested note")]);
        let envoy_transcript = vec![
            Message::new(Role::System, "envoy system"),
            Message::new(Role::User, "envoy task"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                content_blob: None,
                display_content: None,
                reasoning_content: None,
                provider_meta: None,
                tool_calls: Some(vec![nested_call]),
                tool_call_id: None,
                images: None,
                provider: None,
                model: None,
                hidden: false,
                children: None,
                envoy_meta: None,
                origin: None,
            },
            inner_child,
        ];
        let parent =
            Message::tool_result(&call, "[task result]:\nfound it").with_children(envoy_transcript);

        let json = serde_json::to_string_pretty(&parent).unwrap();
        let restored: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.role, Role::Tool);
        assert_eq!(restored.tool_call_id.as_deref(), Some("call_root"));
        let children = restored.children.expect("children round-trip");
        assert_eq!(children.len(), 4);
        // The grep call inside the envoy kept its tool_calls.
        assert!(children[2].tool_calls.is_some());
        // The inner Tool message kept its own nested children (sub-envoy).
        let inner = &children[3];
        assert_eq!(inner.role, Role::Tool);
        assert!(inner.children.is_some(), "sub-envoy children must survive");
        assert_eq!(inner.children.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn with_children_empty_vec_is_none() {
        let call = ToolCall {
            id: "c".to_string(),
            name: "envoy".to_string(),
            arguments: "{}".to_string(),
        };
        let m = Message::tool_result(&call, "x").with_children(Vec::new());
        assert!(
            m.children.is_none(),
            "empty children should collapse to None"
        );
    }

    #[test]
    fn default_message_omits_origin_key() {
        // A genuine message (user input / assistant / tool result) must
        // serialise WITHOUT an `origin` key, so the wire shape is unchanged
        // and legacy consumers / snapshot matchers keep working. Mirrors the
        // `children` compat contract.
        let m = Message::new(Role::User, "hi");
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("origin"),
            "default message must omit origin: {json}"
        );
        assert!(
            !json.contains("injection"),
            "default message must omit injection fields: {json}"
        );
    }

    #[test]
    fn legacy_json_without_origin_loads_as_none() {
        // A pre-C4 snapshot / event-log line has no `origin` key and must
        // deserialise to `origin: None`. This is the load-side of the
        // backward-compat contract.
        let json = r#"{"role":"User","content":"hi","hidden":false}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.content, "hi");
        assert!(m.origin.is_none());
    }

    #[test]
    fn injection_origin_round_trips() {
        // A stamped origin must survive a serialise → deserialise round trip
        // with its kind, reason, and the nested HookEventKind intact. This is
        // the contract that makes the persisted transcript faithfully
        // reconstruct injection provenance.
        use crate::hooks::HookEventKind;
        let msg = Message::injected(
            Role::User,
            "remember X",
            InjectionOrigin::new(InjectionKind::Hook(HookEventKind::PostToolUse))
                .with_reason("my_hook.sh"),
        );
        let json = serde_json::to_string(&msg).unwrap();
        // The origin object is present in the wire form.
        assert!(json.contains("\"origin\""), "origin must serialise: {json}");
        assert!(
            json.contains("\"reason\":\"my_hook.sh\""),
            "reason must serialise: {json}"
        );
        let restored: Message = serde_json::from_str(&json).unwrap();
        let origin = restored.origin.expect("origin round-trip");
        assert_eq!(origin.kind, InjectionKind::Hook(HookEventKind::PostToolUse));
        assert_eq!(origin.reason.as_deref(), Some("my_hook.sh"));
    }

    #[test]
    fn every_injection_kind_serialises_distinctly() {
        // The closed classifier must serialise to distinct wire forms so a
        // persisted transcript can discriminate injection sources without
        // ambiguity. Regression guard: adding a variant without a distinct
        // serde tag would silently collapse provenance. We compare the full
        // serialised `kind` (not just a prefix) because `Hook(HookEventKind)`
        // serialises as a map `{"hook":"session_start"}` while unit variants
        // serialise as a bare string.
        use crate::hooks::HookEventKind;
        let cases: Vec<InjectionKind> = vec![
            InjectionKind::Hook(HookEventKind::SessionStart),
            InjectionKind::Hook(HookEventKind::PostToolUse),
            InjectionKind::Hook(HookEventKind::Stop),
            InjectionKind::Hook(HookEventKind::Turn),
            InjectionKind::PursuitContinuation,
            InjectionKind::PursuitObjectiveUpdated,
            InjectionKind::InterAgent,
            InjectionKind::EnvoySteer,
            InjectionKind::ImplicitSkill,
            InjectionKind::SystemPrompt,
            InjectionKind::CompactionCheckpoint,
            InjectionKind::HiddenTurnInput,
            InjectionKind::LoopReviewNudge,
        ];
        let mut forms = Vec::new();
        for kind in cases {
            // Round-trip each kind in isolation.
            let json = serde_json::to_string(&kind).unwrap();
            let restored: InjectionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, kind, "kind {kind:?} must round-trip");
            forms.push(json);
        }
        let mut sorted = forms.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            forms.len(),
            "injection kinds must serialise to distinct wire forms: {forms:?}"
        );
    }

    #[test]
    fn injected_constructor_stamps_origin_and_hidden() {
        // `Message::injected` must set BOTH hidden=true (display contract) and
        // origin=Some (provenance contract). The two are orthogonal: hidden
        // governs visibility, origin governs "why is this here".
        let m = Message::injected(
            Role::User,
            "nudge",
            InjectionOrigin::new(InjectionKind::LoopReviewNudge),
        );
        assert!(m.hidden, "injected message must be hidden");
        assert!(m.origin.is_some(), "injected message must carry origin");
        assert_eq!(
            m.origin.as_ref().unwrap().kind,
            InjectionKind::LoopReviewNudge
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
}
