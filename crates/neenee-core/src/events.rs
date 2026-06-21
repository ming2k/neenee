//! Wire types for the harness ↔ driver protocol: requests ([`AgentRequest`]),
//! responses ([`AgentResponse`]), live turn events ([`AgentEvent`]), and the
//! small data records they carry.

use crate::{Goal, ImagePart, Message, ToolAccess, ToolOutput, ToolStream};
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
    },
    UserQuestionReply {
        request_id: String,
        answers: Vec<Vec<String>>,
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
}

#[derive(Debug)]
pub enum AgentResponse {
    Text(String),
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
    PermissionsCleared,
    UserQuestionRequest(UserQuestionRequest),
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
    Compacted {
        archived_messages: usize,
        before_chars: usize,
        after_chars: usize,
    },
    HarnessState(HarnessSnapshot),
    GoalUpdated(Goal),
    /// The agent mode changed via `plan_enter` / `plan_exit`.
    ModeChanged(AgentMode),
    /// Plan progress snapshot changed (set by `plan_exit`, mutated by
    /// `update_plan_progress`, cleared by `plan_enter`). Mirrors
    /// [`AgentEvent::PlanProgressUpdated`].
    PlanProgressUpdated(Option<crate::plan::PlanProgress>),
    /// User asked the TUI to open the plan preview modal (via `/plan` or
    /// clicking the sticky panel). Carries the active plan path; the TUI
    /// loads the file content from disk into `App::plan_preview_content`.
    OpenPlanPreview(std::path::PathBuf),
    /// User asked the harness to trigger plan verification (via `/verify`).
    /// The harness turns this into a synthetic hidden prompt that calls
    /// `verify_plan_execution`, so the verifier result lands in the
    /// transcript and the model can act on it.
    TriggerVerification,
    /// The auto-approve toggle changed. Emitted by `/auto-approve` so the TUI
    /// can refresh its badge without waiting for the next harness snapshot.
    AutoApproveChanged(bool),
    RetryScheduled {
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        message: String,
    },
    Activity(String),
    StreamStart,
    StreamDelta(String),
    StreamReasoningDelta(String),
    StreamReasoningEnd(String),
    StreamEnd(String),
    StreamDiscard,
    /// A sub-agent event to render nested inside the parent tool step.
    SubTask {
        parent_call_id: String,
        event: SubTaskEvent,
    },
    /// The turn was paused by a harness guardrail (tool-round budget cap),
    /// not by a runtime failure. Distinct from [`AgentResponse::Error`] so the
    /// UI can render it as a recoverable notice with a continue affordance
    /// instead of a red error. `rounds` is the budget that was reached.
    TurnPaused {
        rounds: usize,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    Build,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSnapshot {
    pub mode: AgentMode,
    pub goal: Option<Goal>,
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

/// Events emitted by a sub-agent spawned through the `task` tool.
///
/// These are forwarded from the child agent back to the parent harness so that
/// the TUI can render nested tool steps and streaming output inside the parent
/// tool step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubTaskEvent {
    /// The sub-agent started a new response stream.
    StreamStart,
    /// New text token from the sub-agent.
    StreamDelta(String),
    /// The sub-agent response stream finished with the final accumulated text.
    StreamEnd(String),
    /// The sub-agent invoked a tool.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// A tool invoked by the sub-agent returned a result.
    ToolResult {
        id: String,
        name: String,
        output: String,
        duration_ms: u64,
    },
    /// A status update from the sub-agent.
    Activity(String),
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
    GoalUpdated(Goal),
    /// The agent mode changed (e.g. via `plan_enter` / `plan_exit`). The TUI
    /// uses this to refresh its mode indicator live, mid-turn.
    ModeChanged(AgentMode),
    /// The plan progress snapshot changed (set by `plan_exit`, mutated by
    /// `update_plan_progress`, cleared by `plan_enter`). The TUI uses this
    /// to refresh the sticky panel above the input box.
    PlanProgressUpdated(Option<crate::plan::PlanProgress>),
    /// The auto-approve toggle changed (via `/auto-approve`).
    AutoApproveChanged(bool),
    PermissionRequest(PermissionRequest),
    UserQuestionRequest(UserQuestionRequest),
    /// A sub-agent spawned by a tool (e.g. `task`) emitted an event.
    SubTask {
        parent_call_id: String,
        event: SubTaskEvent,
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
    /// Short human-friendly title for the prompt (e.g. `"Create goal"`).
    /// Falls back to [`tool`](Self::tool) when a tool does not override
    /// [`Tool::permission_label`]. The TUI renders this as the header.
    #[serde(default)]
    pub label: String,
    /// User-facing description shown in the prompt's "Details" section.
    /// Populated from [`Tool::permission_description`], distinct from the
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
/// classifies origin: `builtin`, `mcp:<server>`, `goal`, or `plan`. `enabled`
/// reflects the session-level enable/disable flag (toggled via
/// [`AgentRequest::ToggleTool`]); disabled tools stay installed but are hidden
/// from the model and rejected if invoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub access: ToolAccess,
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
/// (connected / disabled / failed) is unpacked from [`McpConnectionStatus`] so
/// the DTO stays decoupled from the enum, and `tool_names` carries the
/// per-server tool list that the hint bar collapses to a mere count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub connected: bool,
    pub disabled: bool,
    pub failure: Option<String>,
    pub tool_names: Vec<String>,
}
