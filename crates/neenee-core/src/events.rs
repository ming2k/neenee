//! Wire types for the harness ↔ driver protocol: requests ([`AgentRequest`]),
//! responses ([`AgentResponse`]), live turn events ([`AgentEvent`]), and the
//! small data records they carry.

use crate::{Goal, ImagePart, Message, ToolOutput, ToolStream};
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
    /// Full model-picker state (default id + one row per model) for the
    /// `/models` picker. Supersedes `ProviderKeys` for the picker's needs;
    /// `ProviderKeys` is retained for the header key-readiness summary.
    ModelPicker(ModelPickerSnapshot),
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
    Error(String),
    Exit,
    ProviderSwitched {
        provider: String,
        model: String,
    },
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

/// One row of model-picker state sent from the harness to the TUI. Carries the
/// dynamic per-model signals the picker needs — key readiness, favorite flag,
/// and last-used timestamp — keyed by canonical model id. The TUI joins this
/// with its own static display metadata (`SOLUTIONS`). See ADR-0002.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerRow {
    pub id: String,
    pub key_ready: bool,
    pub favorite: bool,
    /// Unix epoch milliseconds of the last activation. `None` if the model has
    /// never been activated, which the picker sorts as "oldest".
    pub last_used_ms: Option<u64>,
}

/// Full snapshot of model-picker state: which model is the current default plus
/// one row per known model. Sent on startup and after any mutation (favorite
/// toggle, default change, provider switch) so the TUI always renders from a
/// fresh, consistent picture rather than merging incremental updates.
#[derive(Debug, Clone, Default)]
pub struct ModelPickerSnapshot {
    /// Canonical id of the active/default model. Matches `config.default_provider`.
    pub default_id: String,
    pub rows: Vec<ModelPickerRow>,
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
