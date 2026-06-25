//! Wire types for the harness ↔ driver protocol: requests ([`AgentRequest`]),
//! responses ([`AgentResponse`]), live turn events ([`AgentEvent`]), and the
//! small data records they carry.

use crate::{ImagePart, Message, Pursuit, ToolOutput, ToolStream};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
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
        /// request surfaced by a *subagent* (carried up as a
        /// [`TurnEvent::SubAgent`] / [`SubagentEvent::PermissionRequest`]),
        /// this is the parent tool-call id the request was nested under. The
        /// harness looks up the live child's [`crate::SubagentHandle`] in the
        /// task registry by this id and resolves its parked oneshot directly.
        /// `None` means the request came from the top-level (or `/btw` side)
        /// agent and is resolved on `context.agent` as before.
        parent_call_id: Option<String>,
    },
    UserQuestionReply {
        request_id: String,
        answers: Vec<Vec<String>>,
        /// Full-duplex (ADR-0029): the parent tool-call id when the answered
        /// question came from a subagent's `ask_user`
        /// ([`SubagentEvent::UserQuestionRequest`]); `None` for a top-level /
        /// side agent question. See [`AgentRequest::PermissionReply`] for the
        /// routing contract.
        parent_call_id: Option<String>,
    },
    SwitchProvider {
        provider_type: String,
        model: String,
        api_key: Option<String>,
        base_url: Option<String>,
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
    /// Sent by the TUI when the session modal opens.
    QuerySessionContext,
    /// Revoke a single cached "always allow" permission rule. The harness
    /// removes it from the in-memory allowlist and replies with an updated
    /// [`AgentResponse::SessionContext`] so the modal reflects the change.
    RevokePermission {
        tool: String,
        scope: String,
    },
    /// Enable or disable a tool for the current session. Disabled tools are
    /// hidden from the model (their schemas are not sent) and rejected if the
    /// model still tries to call them. The harness replies with an updated
    /// [`AgentResponse::SessionContext`].
    ToggleTool {
        name: String,
        enabled: bool,
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
    /// harness cancels any in-flight turn (same path as [`Interrupt`]) and
    /// replies [`AgentResponse::Exit`], so the normal graceful-exit path runs
    /// (session save + `SessionEnd` hooks) before the process ends and its
    /// background tasks die with it. This is the model's self-initiated
    /// emergency escape hatch, not a user action.
    Abort,
}

#[derive(Debug)]
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
    /// by the harness after [`SessionStore::fork_to_side`] + side `Agent`
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
}

/// The session-scoped shapes a single turn/stream emits, carried under an
/// [`AgentResponse::Turn`] envelope (ADR-0017). Splitting these off
/// `AgentResponse` makes "which session does this belong to" a first-class
/// question: every turn event — whether from the primary or a `/btw` side —
/// arrives tagged with its `session_id`, and global/command responses stay
/// top-level.
#[derive(Debug)]
pub enum TurnEvent {
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
    Compacted {
        archived_messages: usize,
        before_chars: usize,
        after_chars: usize,
    },
    HarnessState(HarnessSnapshot),
    PursuitUpdated(Pursuit),
    /// The active pursuit was cleared (`/pursue clear`, or a session switch
    /// that drops it). A non-gated mirror event: unlike [`HarnessState`],
    /// clearing the pursuit is *not* a turn lifecycle transition, so it must
    /// not touch the activity bar. The TUI uses it to null out the snapshot's
    /// `pursuit` field without flushing the live activity cell.
    PursuitCleared,
    /// The task list changed (full-replace via `todo`, surgical update via
    /// `todo_update`). Mirrors [`AgentEvent::TodosUpdated`]. An empty list
    /// means "no active task list" and hides the sticky panel.
    TodosUpdated(crate::todos::TodoList),
    /// The auto-approve toggle changed. Emitted by `/auto-approve` so the TUI
    /// can refresh its badge without waiting for the next harness snapshot.
    AutoApproveChanged(bool),
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
    /// A subagent event to render nested inside the parent tool step.
    SubAgent {
        parent_call_id: String,
        event: SubagentEvent,
    },
}

/// Coarse status of the primary session, reported to a `/btw` side view's
/// banner (ADR-0017). This is the codex `SideParentStatus` equivalent: the
/// whole reason the parent turn is left running instead of cancelled is so the
/// user can see the main session hit an approval or input wall and jump back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// (`--auto-approve` / `/auto-approve on`). The TUI mirrors this into a
    /// visible badge so the elevated state is never silent.
    pub auto_approve: bool,
}

/// A row in the sessions picker: enough to identify, describe and order a past
/// session without leaking the full transcript to the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOverview {
    pub id: String,
    pub overview: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    pub active: bool,
}

/// One row of provider-picker state sent from the harness to the TUI. Carries
/// the dynamic per-provider signals the picker needs — key readiness, favorite
/// flag, and last-used timestamp — keyed by canonical provider id. The TUI
/// joins this with its own static display metadata. See ADR-0002.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPickerRow {
    pub id: String,
    pub key_ready: bool,
    pub favorite: bool,
    /// Unix epoch milliseconds of the last activation. `None` if the provider
    /// has never been activated, which the picker sorts as "oldest".
    pub last_used_ms: Option<u64>,
}

/// Full snapshot of provider-picker state: which provider is the current
/// default plus one row per known provider. Sent on startup and after any
/// mutation (favorite toggle, default change, provider switch) so the TUI
/// always renders from a fresh, consistent picture rather than merging
/// incremental updates.
#[derive(Debug, Clone, Default)]
pub struct ProviderPickerSnapshot {
    /// Canonical id of the active/default provider. Matches
    /// `config.default_provider`.
    pub default_id: String,
    pub rows: Vec<ProviderPickerRow>,
}

/// Events emitted by a subagent spawned through the `task` tool.
///
/// These are forwarded from the child agent back to the parent harness so that
/// the TUI can render nested tool steps and streaming output inside the parent
/// tool step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentEvent {
    /// Emitted once at subagent start, carrying the bound profile's name
    /// (e.g. `"explore"`, `"plan"`, `"verify"`). Lets the TUI label the
    /// subagent by its role rather than a generic "Subagent", so a user can
    /// tell a planning subagent from a research one at a glance.
    Started { profile: &'static str },
    /// The subagent started a new response stream.
    StreamStart,
    /// New text token from the subagent.
    StreamDelta(String),
    /// The subagent response stream finished with the final accumulated text.
    StreamEnd(String),
    /// The subagent invoked a tool.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// A tool invoked by the subagent returned a result.
    ToolResult {
        id: String,
        name: String,
        output: String,
        duration_ms: u64,
    },
    /// A status update from the subagent.
    Activity(String),
    /// The subagent's permission broker surfaced a write/execute tool call
    /// that needs a human decision. Full-duplex (ADR-0029): this carries the
    /// request *up* to the parent harness so the user can answer it; the
    /// reply travels back *down* through the subagent handle's
    /// `reply_permission` (resolving the parked oneshot directly), unblocking
    /// the subagent's pending tool. Only fires when
    /// the subagent's profile does not suppress the broker (e.g. via
    /// `auto_approve`) — a read-only profile never produces one.
    PermissionRequest(PermissionRequest),
    /// The subagent called `ask_user` and is blocked awaiting answers.
    /// Full-duplex (ADR-0029): carries the questions *up*; the reply travels
    /// back *down* through the subagent handle's `reply_user_question`. Only
    /// fires for profiles with `allow_user_interaction: true`.
    UserQuestionRequest(UserQuestionRequest),
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
/// the live stream). The top-level agent and spawned sub-agents share the same
/// `Op` vocabulary — a subagent is just an agent whose inbox sender the
/// parent holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOp {
    /// Append a visible user message to the live transcript before the next
    /// model request, as if the user typed it. Lets a parent (or, for a
    /// subagent, the orchestrating agent) steer a running turn with new
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

#[derive(Debug, Clone)]
pub enum AgentEvent {
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
    /// The auto-approve toggle changed (via `/auto-approve`).
    AutoApproveChanged(bool),
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
    /// A subagent spawned by a tool (e.g. `task`) emitted an event.
    SubAgent {
        parent_call_id: String,
        event: SubagentEvent,
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

/// A complete, render-ready picture of the live session, sent from the harness
/// to the TUI for the session-context modal. Every pane in that modal reads
/// from this one snapshot, so opening the modal and any mutation
/// (revoke / toggle) only needs a single request/response round-trip rather
/// than one per pane. Built by the harness from its own state (provider,
/// tools, permissions, skills) plus the MCP load result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub source: String,
}

/// One cached "always allow" permission rule, shown in the modal's Permissions
/// pane where it can be revoked individually via [`AgentRequest::RevokePermission`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionRuleInfo {
    pub tool: String,
    pub scope: String,
}

/// One skill in the registry, shown in the modal's Skills pane. `source` is the
/// [`SkillScope`](../neenee_agent/skills/struct.SkillScope.html) display string
/// (system / remote / user / extra / repo).
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub connected: bool,
    pub disabled: bool,
    pub failure: Option<String>,
    pub tool_names: Vec<String>,
}
