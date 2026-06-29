//! Wire types for the harness ↔ driver protocol: requests ([`AgentRequest`]),
//! responses ([`AgentResponse`]), live turn events ([`AgentEvent`]), and the
//! small data records they carry.

use crate::{ImagePart, Message, NudgeConfig, Pursuit, ToolOutput, ToolStream};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentRequest {
    Chat {
        text: String,
        images: Vec<ImagePart>,
    },
    SlashCommand(String),
    Interrupt,
    PermissionReply {
        request_id: String,
        decision: PermissionDecision,
        /// Full-duplex (ADR-0029): when the reply targets a permission
        /// request surfaced by a *envoy* (carried up as a
        /// [`TurnEvent::Envoy`] / [`EnvoyEvent::PermissionRequest`]),
        /// this is the parent tool-call id the request was nested under. The
        /// harness looks up the live child's `crate::EnvoyHandle` in the
        /// task registry by this id and resolves its parked oneshot directly.
        /// `None` means the request came from the top-level (or `/btw` side)
        /// agent and is resolved on `context.agent` as before.
        parent_call_id: Option<String>,
    },
    UserQuestionReply {
        request_id: String,
        answers: Vec<Vec<String>>,
        /// Full-duplex (ADR-0029): the parent tool-call id when the answered
        /// question came from an envoy's `ask_user`
        /// ([`EnvoyEvent::UserQuestionRequest`]); `None` for a top-level /
        /// side agent question. See [`AgentRequest::PermissionReply`] for the
        /// routing contract.
        parent_call_id: Option<String>,
    },
    /// Reply to an [`AgentEvent::InputRequest`] (L3.5 β): the operator's input
    /// for an interactive `bash` command, routed back to the parked oneshot.
    /// `parent_call_id` mirrors the question/permission replies for envoy
    /// routing.
    InputReply {
        request_id: String,
        text: String,
        parent_call_id: Option<String>,
    },
    SwitchProvider {
        provider_type: String,
        model: String,
        api_key: Option<String>,
        base_url: Option<String>,
    },
    /// Add a user-defined provider from a TUI template, persist it to config,
    /// then activate it. `protocol` is one of `"openai"` | `"anthropic"` |
    /// `"gemini"`; `api_key` may be empty (a keyless OpenAI-compatible relay
    /// suppresses the auth header). The harness derives a stable id from `name`.
    /// `models` is the provider's seeded model list — one channel per model,
    /// the first becoming the default/active model. A template that seeds the
    /// whole Claude family lands all of them in the picker's stage-2 list.
    ///
    /// Per ADR-0046, reasoning (effort/thinking) is no longer set at provider
    /// creation — it is opted in per model via the stage-2 model `e` editor
    /// (`EditProviderModel`). New channels start with thinking off.
    AddProvider {
        name: String,
        protocol: String,
        base_url: String,
        api_key: String,
        models: Vec<String>,
    },
    /// Edit a user-defined provider's metadata in place (display name, protocol,
    /// base URL, API key) without touching its model list — every channel keeps
    /// its model id, so a multi-model custom provider is not collapsed. An empty
    /// `api_key` leaves the existing key untouched. Built-in providers are not
    /// editable this way (their `e` editor only sets the API key).
    ///
    /// Per ADR-0046, this no longer carries reasoning knobs — effort/thinking
    /// are per-model (`EditProviderModel`), not provider-wide.
    EditProvider {
        id: String,
        name: String,
        protocol: String,
        base_url: String,
        api_key: String,
    },
    /// Append a model to an existing user-defined provider (a new channel sharing
    /// the provider's transport/endpoint/key), persist, and push a fresh picker
    /// snapshot. Built-in providers reject this (curated model lists).
    AddProviderModel {
        provider_id: String,
        model: String,
    },
    /// Remove a model (channel) from a user-defined provider, persist, and push a
    /// fresh picker snapshot. The last remaining model is kept (a provider must
    /// serve at least one model).
    RemoveProviderModel {
        provider_id: String,
        model: String,
    },
    /// Edit settings for one model/channel of a user-defined provider. This is
    /// intentionally channel-scoped: Anthropic effort/thinking can vary by
    /// model even when the provider endpoint/key are shared.
    EditProviderModel {
        provider_id: String,
        model: String,
        effort: Option<String>,
        thinking: Option<bool>,
    },
    /// Edit the per-model reasoning settings (Anthropic effort/thinking) for a
    /// **built-in** model, persisted into the `[model_reasoning."<model-id>"]`
    /// table. This is the model-level counterpart to `EditProviderModel`:
    /// built-in providers (e.g. `anthropic`) have no user-editable channels, so
    /// their per-model reasoning knobs live in this shared table keyed by model
    /// id rather than on a channel. ADR-0045.
    EditModelReasoning {
        model: String,
        effort: Option<String>,
        thinking: Option<bool>,
    },
    /// Delete a user-defined provider entirely: drop the entry from
    /// `config.providers`, remove it from `favorites`, and persist. If the
    /// deleted provider was active (`default_provider`), fall back to the
    /// default built-in provider (`"kimi-code"`) and activate it so the live
    /// provider never points at a removed entry. Built-in providers are not
    /// deletable this way; the handler ignores unknown / built-in ids.
    DeleteProvider {
        id: String,
    },
    /// Toggle the favorite flag on a model in the picker. The id is
    /// canonicalized by the harness before it touches config.
    ToggleFavorite {
        id: String,
    },
    /// Make `id` the default model and activate it. Equivalent to selecting it
    /// in the picker and pressing `d`: it both sets the persisted default and
    /// switches the live provider.
    SetDefaultModel {
        id: String,
    },
    /// Delete a session (active or archived) by id or short id prefix.
    DeleteSession {
        id: String,
    },
    /// Request a fresh session-context snapshot (model / tools / permissions /
    /// skills / mcp). The harness replies with [`AgentResponse::SessionContext`].
    /// Sent by the TUI when a manager modal opens.
    QuerySessionContext,
    /// Revoke a single cached "always allow" permission rule. The harness
    /// removes it from the in-memory allowlist and replies with an updated
    /// [`AgentResponse::SessionContext`] so the modal reflects the change.
    RevokePermission {
        tool: String,
        scope: String,
    },
    /// Clear every cached "always allow" permission rule for this process.
    /// The harness drops the whole in-memory allowlist and replies with an
    /// updated [`AgentResponse::SessionContext`] so the permissions manager
    /// modal reflects the now-empty list.
    ClearAllPermissions,
    /// Enable or disable a tool for the current session. Disabled tools are
    /// hidden from the model (their schemas are not sent) and rejected if the
    /// model still tries to call them. The harness replies with an updated
    /// [`AgentResponse::SessionContext`].
    ToggleTool {
        name: String,
        enabled: bool,
    },
    /// Enable or disable a configured MCP server for the live session. Unlike
    /// [`AgentRequest::ToggleTool`] (which only flips a session flag on an
    /// already-installed tool), this connects/disconnects the server: disabling
    /// drops its tools from the live tool list and closes the connection;
    /// enabling reconnects it from `[mcp.<name>]` config and re-discovers its
    /// tools. Session-scoped — config.toml is not rewritten, so a restart
    /// restores the configured state. The harness replies with an updated
    /// [`AgentResponse::SessionContext`].
    ToggleMcpServer {
        name: String,
        enabled: bool,
    },
    /// Reset and re-establish one MCP server's connection, re-discovering its
    /// tools (the per-server analogue of the periodic catalog refresh). Used by
    /// the `/mcp` modal's `r` action to recover a crashed/failed server on
    /// demand. The harness replies with an updated
    /// [`AgentResponse::SessionContext`].
    ReconnectMcpServer {
        name: String,
    },
    /// Run a shell command directly through the `bash` tool, bypassing the
    /// LLM. Triggered by the TUI's `!` prefix (e.g. `!git status`). The
    /// harness emits a synthetic `ToolCall`, live `ToolStream` events, and a
    /// final `ToolResult`, mirroring a normal bash step's lifecycle.
    ShellCommand {
        command: String,
    },
    /// Leave the active `/btw` side conversation and return to the primary
    /// view (ADR-0017). The harness cancels any in-flight side turn, drops
    /// the live side session (the side file stays on disk, recoverable via
    /// `/sessions`), and emits [`AgentResponse::SideViewClosed`]. Sent by
    /// the TUI when the user presses `Esc` / `Ctrl+C` inside the side view.
    ExitSideView,
    /// Abort the current operation and exit the program gracefully. Sent by
    /// the model-facing `abort` tool when it detects a stuck state — a loop,
    /// a dangerous operation, or a dead end it cannot recover from. The
    /// harness cancels any in-flight turn (same path as `Interrupt`) and
    /// replies [`AgentResponse::Exit`], so the normal graceful-exit path runs
    /// (session save + `SessionEnd` hooks) before the process ends and its
    /// background tasks die with it. This is the model's self-initiated
    /// emergency escape hatch, not a user action.
    Abort,
    /// Update the nudge configuration at runtime (from the `/config` modal).
    /// The harness writes the new config to `config.toml`, calls
    /// `Agent::set_nudge_config`, and replies with
    /// [`AgentResponse::NudgeConfigUpdated`] so the modal reflects the
    /// persisted state. The `enabled` flag takes effect on the next round
    /// boundary; the thresholds take effect on the next turn (per-turn guard
    /// state is rebuilt fresh each turn).
    UpdateNudgeConfig(NudgeConfig),
    /// Update the transcript layout preference (from the `/config` modal).
    /// The harness writes the new value to `config.toml`'s `[tui]
    /// transcript_layout` and replies with [`AgentResponse::TuiLayoutUpdated`]
    /// carrying the persisted string so the modal re-renders from the
    /// authoritative state. The value is a raw config string ("compact" /
    /// "round_band"); interpretation into a [`crate`] layout `Strategy`
    /// happens in the renderer, keeping the core free of render types.
    UpdateTuiLayout(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentResponse {
    /// A per-turn event tagged with the session it belongs to (ADR-0017). The
    /// TUI keys its transcript buffers by `session_id` and routes `event` to
    /// the matching one, so a primary turn and a live `/btw` side turn can
    /// stream concurrently over the single harness↔TUI channel without
    /// clobbering each other's transcript.
    ///
    /// Global (non-session-scoped) responses — command replies, modal
    /// snapshots, provider switches — stay as dedicated top-level variants so
    /// they are handled once, regardless of which view is focused.
    Turn {
        session_id: String,
        event: TurnEvent,
    },
    /// Coarse status of the primary session, surfaced to a side view's banner
    /// while the user is inside a `/btw`. Emitted by the session registry's
    /// parent-status watcher; the primary turn is deliberately left running, so
    /// this is how the user learns the main session hit an approval/input wall.
    ParentStatus(ParentStatus),
    /// The user entered a `/btw` side conversation (ADR-0017). The TUI seeds
    /// an empty side transcript buffer keyed by `side_id`, switches to the
    /// side view, and records `primary_id` so per-turn events route by
    /// `session_id` (primary → primary buffer, side → side buffer). Emitted
    /// by the harness after `SessionStore::fork_to_side` + side `Agent`
    /// construction succeed.
    SideViewOpened {
        side_id: String,
        primary_id: String,
    },
    /// The user left the `/btw` side view (ADR-0017). The TUI returns to the
    /// primary transcript and clears the side buffer. Emitted by the harness
    /// in reply to [`AgentRequest::ExitSideView`] once the live side session
    /// has been torn down.
    SideViewClosed,
    PermissionsCleared,
    /// Lowercase provider name → whether a usable API key is configured.
    ProviderKeys(Vec<(String, bool)>),
    /// Full provider-picker state (default id + one row per provider) for the
    /// provider picker. Supersedes `ProviderKeys` for the picker's needs;
    /// `ProviderKeys` is retained for the header key-readiness summary.
    ProviderPicker(ProviderPickerSnapshot),
    ConversationCleared,
    ConversationReplaced(Vec<Message>),
    /// Replace the sessions picker contents (and open the picker).
    SessionsOverview(Vec<SessionOverview>),
    Error(String),
    Exit,
    ProviderSwitched {
        provider: String,
        model: String,
    },
    /// Full session-context snapshot (model + tools + permissions + skills +
    /// mcp) for the session modal. Sent in reply to [`AgentRequest::QuerySessionContext`]
    /// and re-sent after any mutation handled by the harness
    /// ([`AgentRequest::RevokePermission`] / [`AgentRequest::ToggleTool`]).
    SessionContext(SessionContextSnapshot),
    /// The nudge configuration was updated (from the `/config` modal via
    /// [`AgentRequest::UpdateNudgeConfig`]). Carries the persisted config so
    /// the modal re-renders from the authoritative state — the TOML write
    /// is the source of truth, not the TUI's optimistic local edit.
    NudgeConfigUpdated(NudgeConfig),
    /// The transcript layout preference was updated (from the `/config` modal
    /// via [`AgentRequest::UpdateTuiLayout`]). Carries the persisted config
    /// string so the modal re-renders from the authoritative state — the TOML
    /// write is the source of truth, not the TUI's optimistic local edit.
    TuiLayoutUpdated(String),
}

/// A user-visible notice emitted by the agent or harness.
///
/// This is distinct from state-sync events such as [`TurnEvent::TodosUpdated`]
/// and blocking interaction events such as [`TurnEvent::PermissionRequest`]:
/// those events update UI state or require a reply, while a notice means
/// "surface this fact to the user".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNotice {
    pub id: String,
    pub kind: NoticeKind,
    pub severity: NoticeSeverity,
    /// Preferred UI surface. Frontends may degrade this when a surface is not
    /// available, e.g. render a toast as an inline notice.
    pub surface: NoticeSurface,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub source: NoticeSource,
}

impl AgentNotice {
    pub fn new(
        kind: NoticeKind,
        severity: NoticeSeverity,
        title: impl Into<String>,
        source: NoticeSource,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            severity,
            surface: NoticeSurface::Inline,
            title: title.into(),
            body: None,
            source,
        }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn with_surface(mut self, surface: NoticeSurface) -> Self {
        self.surface = surface;
        self
    }

    pub fn render_text(&self) -> String {
        match self.body.as_deref().filter(|body| !body.trim().is_empty()) {
            Some(body) => format!("{}\n{}", self.title, body),
            None => self.title.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    ProviderRetry,
    NudgeInjected,
    ReviewAlert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSurface {
    /// Render inline in the current conversation or event feed.
    Inline,
    /// Show as a transient bubble/toast.
    Toast,
    /// Show in a retained alert area until the related condition clears or is
    /// superseded.
    Banner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSource {
    Agent,
    TurnGuard,
    Todo,
    Review,
    Pursuit,
    Harness,
}

/// The session-scoped shapes a single turn/stream emits, carried under an
/// [`AgentResponse::Turn`] envelope (ADR-0017). Splitting these off
/// `AgentResponse` makes "which session does this belong to" a first-class
/// question: every turn event — whether from the primary or a `/btw` side —
/// arrives tagged with its `session_id`, and global/command responses stay
/// top-level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnEvent {
    Notice(AgentNotice),
    Text(String),
    /// Turn-level error (e.g. a provider failure mid-turn). Distinct from the
    /// global [`AgentResponse::Error`] only in that it belongs to a specific
    /// session's transcript and is therefore carried under the [`Turn`]
    /// envelope.
    ///
    /// [`Turn`]: AgentResponse::Turn
    Error(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        structured: ToolOutput,
        duration_ms: u64,
    },
    /// Incremental output streamed by a running tool (see [`ToolStream`]).
    ToolStream {
        id: String,
        stream: ToolStream,
    },
    ToolCancelled {
        id: String,
        name: String,
    },
    PermissionRequest(PermissionRequest),
    UserQuestionRequest(UserQuestionRequest),
    /// Mirrors [`AgentEvent::InputRequest`]: an interactive `bash` command
    /// needs operator input (L3.5 β).
    InputRequest(InputRequest),
    Compacted {
        archived_messages: usize,
        before_chars: usize,
        after_chars: usize,
    },
    HarnessState(HarnessSnapshot),
    PursuitUpdated(Pursuit),
    /// The active pursuit was cleared (`/pursue clear`, or a session switch
    /// that drops it). A non-gated mirror event: unlike `HarnessState`,
    /// clearing the pursuit is *not* a turn lifecycle transition, so it must
    /// not touch the activity bar. The TUI uses it to null out the snapshot's
    /// `pursuit` field without flushing the live activity cell.
    PursuitCleared,
    /// The task list changed (full-replace via `todo`, surgical update via
    /// `todo_update`). Mirrors [`AgentEvent::TodosUpdated`]. An empty list
    /// means "no active task list" and hides the sticky panel.
    TodosUpdated(crate::todos::TodoList),
    /// The unattended toggle changed. Emitted by `/unattended` so the TUI
    /// can refresh its badge without waiting for the next harness snapshot.
    UnattendedChanged(bool),
    /// Mirrors [`AgentEvent::SessionReview`]. The TUI renders a non-modal
    /// alert with `alert` (or clears it when `alert` is empty).
    SessionReview {
        alert: String,
    },
    RetryScheduled {
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        message: String,
    },
    Activity(String),
    /// A new tool round started within the current turn. `round` is the
    /// 0-indexed model-request index within the turn (0 = the first request).
    /// Surfaced as structured data so the activity bar can render
    /// `turn N · round M · <status>` without parsing the round back out of the
    /// `Activity` status string. Emitted just before the matching
    /// `Activity("waiting for model")`.
    RoundStarted {
        round: usize,
    },
    StreamStart,
    StreamDelta(String),
    StreamReasoningDelta(String),
    StreamReasoningEnd(String),
    StreamEnd(String),
    StreamDiscard,
    /// An envoy event to render nested inside the parent tool step.
    Envoy {
        parent_call_id: String,
        event: EnvoyEvent,
    },
}

/// Coarse status of the primary session, reported to a `/btw` side view's
/// banner (ADR-0017). This is the codex `SideParentStatus` equivalent: the
/// whole reason the parent turn is left running instead of cancelled is so the
/// user can see the main session hit an approval or input wall and jump back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParentStatus {
    Idle,
    Running,
    NeedsApproval,
    NeedsInput,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSnapshot {
    pub pursuit: Option<Pursuit>,
    pub loop_status: String,
    /// Whether write-tool permission prompts are bypassed this session
    /// (`--unattended` / `/unattended on`). The TUI mirrors this into a
    /// visible badge so the elevated state is never silent.
    pub unattended: bool,
}

/// A row in the sessions picker: enough to identify, describe and order a past
/// session without leaking the full transcript to the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOverview {
    pub id: String,
    pub overview: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    pub active: bool,
}

/// One row of provider-picker state sent from the harness to the TUI. Carries
/// everything the picker renders for a provider — display name, the served model
/// ids and the active one, plus the dynamic signals (key readiness, favorite,
/// last-used) — keyed by canonical provider id. The TUI renders directly from
/// these rows (built-in and user-defined providers share one path), so no static
/// per-provider table is consulted. See ADR-0002.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPickerRow {
    pub id: String,
    /// Display name (e.g. `"OpenAI"`, `"Anthropic"`, or a custom provider's name).
    pub name: String,
    /// Wire id of the currently-active model on this provider.
    pub model: String,
    /// Every model id this provider serves, in catalog order. A single-model
    /// provider lists exactly one; multi-model providers list all of them.
    pub models: Vec<String>,
    /// Per-model/channel settings in the same order as `models`. Newer TUIs use
    /// this to render and edit model-specific controls such as Anthropic
    /// effort/thinking. `models` stays as the simple compatibility list.
    #[serde(default)]
    pub model_info: Vec<ProviderModelInfo>,
    /// `true` for built-in presets, `false` for user-defined providers. The TUI
    /// only offers add/remove-model (and full meta editing) on user-defined
    /// providers.
    pub builtin: bool,
    /// Wire protocol id of the default channel (`"openai"` | `"anthropic"` |
    /// `"gemini"`), used to pre-fill the edit form for a user-defined provider.
    /// Empty for built-ins (their `e` editor only changes the API key).
    pub protocol: String,
    /// Base URL of the default channel, used to pre-fill the edit form. Empty
    /// for built-ins and keyless/native transports.
    pub base_url: String,
    pub key_ready: bool,
    pub favorite: bool,
    /// Unix epoch milliseconds of the last activation. `None` if the provider
    /// has never been activated, which the picker sorts as "oldest".
    pub last_used_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    /// Wire model id. Mirrors an entry in [`ProviderPickerRow::models`].
    pub model: String,
    /// Wire protocol id of the channel serving this model (`"openai"` |
    /// `"anthropic"` | `"gemini"`).
    pub protocol: String,
    /// Effective reasoning effort for Anthropic-protocol channels. `None` for
    /// protocols that do not expose an effort knob.
    pub effort: Option<String>,
    /// Effective extended-thinking state for Anthropic-protocol channels.
    /// `None` for protocols that do not expose a thinking knob.
    pub thinking: Option<bool>,
}

/// Full snapshot of provider-picker state: which provider is the current
/// default plus one row per known provider. Sent on startup and after any
/// mutation (favorite toggle, default change, provider switch) so the TUI
/// always renders from a fresh, consistent picture rather than merging
/// incremental updates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderPickerSnapshot {
    /// Canonical id of the active/default provider. Matches
    /// `config.default_provider`.
    pub default_id: String,
    pub rows: Vec<ProviderPickerRow>,
}

/// Events emitted by an envoy spawned through the `task` tool.
///
/// These are forwarded from the child agent back to the parent harness so that
/// the TUI can render nested tool steps and streaming output inside the parent
/// tool step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvoyEvent {
    /// Emitted once at envoy start, carrying the bound profile's name
    /// (e.g. `"explore"`, `"plan"`, `"verify"`). Lets the TUI label the
    /// envoy by its role rather than a generic "Envoy", so a user can
    /// tell a planning envoy from a research one at a glance.
    Started { profile: String },
    /// A user-visible notice from the envoy.
    Notice(AgentNotice),
    /// The envoy started a new response stream.
    StreamStart,
    /// New text token from the envoy.
    StreamDelta(String),
    /// The envoy response stream finished with the final accumulated text.
    StreamEnd(String),
    /// The envoy invoked a tool.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// A tool invoked by the envoy returned a result.
    ToolResult {
        id: String,
        name: String,
        output: String,
        duration_ms: u64,
    },
    /// A status update from the envoy.
    Activity(String),
    /// The envoy's permission broker surfaced a write/execute tool call
    /// that needs a human decision. Full-duplex (ADR-0029): this carries the
    /// request *up* to the parent harness so the user can answer it; the
    /// reply travels back *down* through the envoy handle's
    /// `reply_permission` (resolving the parked oneshot directly), unblocking
    /// the envoy's pending tool. Only fires when
    /// the envoy's profile does not suppress the broker (e.g. via
    /// `unattended`) — a read-only profile never produces one.
    PermissionRequest(PermissionRequest),
    /// The envoy called `ask_user` and is blocked awaiting answers.
    /// Full-duplex (ADR-0029): carries the questions *up*; the reply travels
    /// back *down* through the envoy handle's `reply_user_question`. Only
    /// fires for profiles with `allow_user_interaction: true`.
    UserQuestionRequest(UserQuestionRequest),
    /// The envoy's `bash` tool classified a command interactive and needs
    /// operator input (L3.5 β). Carries the request *up*; the reply travels
    /// back *down* through the envoy handle's `reply_input`.
    InputRequest(InputRequest),
}

/// Steering operations a parent can submit into a running agent's inbox — the
/// down-direction of full-duplex (ADR-0029). Distinct from the request/reply
/// class ([`PermissionRequest`] / [`UserQuestionRequest`]), which resolve
/// instantly via the agent's shared-state oneshots (`reply_permission` /
/// `reply_user_question`) and therefore do **not** flow through this queue: a
/// reply must unblock a tool that is parked mid-turn, so it cannot wait for
/// the driver loop to drain. This enum covers only the "new input / control"
/// class that is safe to apply at the next tool-round boundary.
///
/// Modeled on codex's `Op` (`codex-rs/protocol/src/protocol.rs`), trimmed to
/// neenee's driver shape: the agent owns an `mpsc` inbox whose receiver is
/// drained at the top of every tool round (and, for `Interrupt`, raced against
/// the live stream). The top-level agent and spawned envoys share the same
/// `Op` vocabulary — an envoy is just an agent whose inbox sender the
/// parent holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentOp {
    /// Append a visible user message to the live transcript before the next
    /// model request, as if the user typed it. Lets a parent (or, for a
    /// envoy, the orchestrating agent) steer a running turn with new
    /// information without restarting it. codex `inject_if_running` analogue.
    InjectUserMessage(String),
    /// Append a hidden (system-level) steering note — like
    /// [`AgentOp::InjectUserMessage`] but recorded as a hidden user message so
    /// it informs the model without polluting the visible transcript. codex
    /// `InterAgentCommunication` analogue.
    InterAgentMessage { msg: String },
    /// Abort the current turn at the next boundary. Coarser than the parent's
    /// `CancellationToken` (which cancels instantly): this is the
    /// handle-addressable path for a caller that owns the inbox but not the
    /// cancel token. codex `Op::Interrupt` analogue.
    Interrupt,
    /// Tear the agent down (interrupt + signal that the shutdown was
    /// requested rather than cancelled). codex `Op::Shutdown` analogue.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    Notice(AgentNotice),
    ModelRequestStarted {
        tool_round: usize,
    },
    AssistantDelta {
        delta: String,
        start: bool,
    },
    AssistantEnd(String),
    AssistantDiscard,
    ReasoningDelta {
        delta: String,
        start: bool,
    },
    ReasoningEnd(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        structured: ToolOutput,
        duration_ms: u64,
    },
    /// Incremental output streamed by a running tool (see [`ToolStream`]).
    ToolStream {
        id: String,
        stream: ToolStream,
    },
    ToolCancelled {
        id: String,
        name: String,
    },
    PursuitUpdated(Pursuit),
    /// The task list changed (`todo` / `todo_update`). The TUI uses this to refresh the
    /// unified sticky panel above the input box.
    TodosUpdated(crate::todos::TodoList),
    /// The unattended toggle changed (via `/unattended`).
    UnattendedChanged(bool),
    /// An on-demand session-review diagnostic ran (ADR-0018, superseding the
    /// periodic ADR-0016 design). `alert` is a pre-rendered, human-facing
    /// summary of the worst verdict across all review dimensions (empty string
    /// when the turn is healthy — the TUI treats empty as "clear any prior
    /// alert"). Surfaced as a non-modal banner so the user can decide whether
    /// to interrupt; it does not abort the turn unless an opt-in
    /// `hard_stop_rounds` budget is configured.
    SessionReview {
        alert: String,
    },
    PermissionRequest(PermissionRequest),
    UserQuestionRequest(UserQuestionRequest),
    /// An interactive `bash` command needs a line of input from the operator
    /// (L3.5 β). The TUI shows an inline input panel; the reply travels back
    /// as [`AgentRequest::InputReply`].
    InputRequest(InputRequest),
    /// An envoy spawned by a tool (e.g. `task`) emitted an event.
    Envoy {
        parent_call_id: String,
        event: EnvoyEvent,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub tool: String,
    /// Short human-friendly title for the prompt (e.g. `"Create pursuit"`).
    /// Falls back to [`tool`](Self::tool) when a tool does not override
    /// `Tool::permission_label`. The TUI renders this as the header.
    #[serde(default)]
    pub label: String,
    /// User-facing description shown in the prompt's "Details" section.
    /// Populated from `Tool::permission_description`, distinct from the
    /// model-facing `Tool::description`.
    pub description: String,
    pub arguments: String,
    pub scope: String,
}

/// One option offered to the user inside an `ask_user` question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionOption {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A single question inside an `ask_user` tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestion {
    /// Short label shown as a chip/tag above the question (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The full question text.
    pub question: String,
    /// Available choices. Must contain at least one option.
    pub options: Vec<UserQuestionOption>,
    /// Whether the user may select more than one option.
    #[serde(default)]
    pub multi_select: bool,
}

/// Request sent from the agent to the TUI when the model calls `ask_user`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionRequest {
    pub id: String,
    pub questions: Vec<UserQuestion>,
}

/// Reply sent from the TUI back to the agent after the user answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionReply {
    pub request_id: String,
    /// One array of selected option labels per question.
    pub answers: Vec<Vec<String>>,
}

/// Request sent from the agent to the TUI when a `bash` command is classified
/// interactive and needs a line of input the agent cannot supply itself
/// (L3.5 β — the default human-input path). The TUI shows an inline input
/// panel; the operator's reply is sent back as an [`InputReply`]. If the
/// operator dismisses it (Esc), an empty reply cancels the command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRequest {
    pub id: String,
    /// The command that needs input, shown for context.
    pub command: String,
    /// A human-readable prompt describing what to enter (e.g. "sudo password",
    /// "passphrase", "confirmation").
    pub prompt: String,
    /// Whether to mask the typed input (passwords/passphrases).
    pub secret: bool,
}

/// Reply sent from the TUI back to the agent carrying the operator's input.
/// An empty `text` signals cancellation (the command runs with closed stdin
/// and fails fast with a non-interactive remedy hint).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputReply {
    pub request_id: String,
    pub text: String,
}

/// A complete, render-ready picture of the live session, sent from the harness
/// to the TUI for the session-context modal. Every pane in that modal reads
/// from this one snapshot, so opening the modal and any mutation
/// (revoke / toggle) only needs a single request/response round-trip rather
/// than one per pane. Built by the harness from its own state (provider,
/// tools, permissions, skills) plus the MCP load result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContextSnapshot {
    pub model: ModelInfo,
    pub tools: Vec<ToolInfo>,
    pub permissions: Vec<PermissionRuleInfo>,
    pub skills: Vec<SkillInfo>,
    pub mcp: Vec<McpServerInfo>,
}

/// Model-side pane of [`SessionContextSnapshot`]. `capabilities` carries
/// heuristic hints (e.g. "tool calling", "reasoning") since per-model
/// capability data is not yet modeled in the catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub model: String,
    pub display_name: String,
    pub context_window: usize,
    pub api_key_ready: bool,
    pub description: String,
    pub capabilities: Vec<String>,
}

/// One tool in the session, as seen by the modal's Tools pane. `source`
/// classifies origin: `builtin`, `mcp:<server>`, `pursuit`, or `plan`. `enabled`
/// reflects the session-level enable/disable flag (toggled via
/// [`AgentRequest::ToggleTool`]); disabled tools stay installed but are hidden
/// from the model and rejected if invoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub source: String,
}

/// One cached "always allow" permission rule, shown in the modal's Permissions
/// pane where it can be revoked individually via [`AgentRequest::RevokePermission`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRuleInfo {
    pub tool: String,
    pub scope: String,
}

/// One skill in the registry, shown in the modal's Skills pane. `source` is the
/// [`SkillScope`](../neenee_agent/skills/struct.SkillScope.html) display string
/// (system / remote / user / extra / repo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub enabled: bool,
    pub source: String,
    pub tags: Vec<String>,
}

/// One MCP server, shown in the modal's MCP pane. The connection tri-state
/// (connected / disabled / failed) is unpacked from
/// [`crate::mcp::McpConnectionStatus`] so the DTO stays decoupled from the
/// enum, and `tool_names` carries the per-server tool list that the hint bar
/// collapses to a mere count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub connected: bool,
    pub disabled: bool,
    pub failure: Option<String>,
    pub tool_names: Vec<String>,
}
