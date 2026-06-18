pub use async_trait::async_trait;
use futures::{future::join_all, stream::BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub mod goals;
pub use goals::{
    Goal, GoalAccountingResult, GoalChecklistItem, GoalChecklistStatus, GoalService, GoalStatus,
    GoalStore, TokenUsage, TurnOutcome, TurnTimer,
};

const MAX_TOOL_ROUNDS: usize = 32;
const MAX_REPEATED_TOOL_CALLS: usize = 3;
pub const GOAL_COMPLETE_MARKER: &str = "[NEENEE_GOAL_COMPLETE]";
const RETRYABLE_ERROR_PREFIX: &str = "[NEENEE_RETRYABLE]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryableError {
    pub message: String,
    pub retry_after_ms: Option<u64>,
}

pub fn retryable_error(message: impl Into<String>, retry_after_ms: Option<u64>) -> String {
    let error = RetryableError {
        message: message.into(),
        retry_after_ms,
    };
    format!(
        "{}{}",
        RETRYABLE_ERROR_PREFIX,
        serde_json::to_string(&error).unwrap_or_else(|_| "{}".to_string())
    )
}

pub fn parse_retryable_error(error: &str) -> Option<RetryableError> {
    serde_json::from_str(error.strip_prefix(RETRYABLE_ERROR_PREFIX)?).ok()
}

pub fn public_error_message(error: &str) -> String {
    parse_retryable_error(error)
        .map(|retry| retry.message)
        .unwrap_or_else(|| error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Inline image attachments (typically pasted into the prompt). Each part
    /// carries a MIME type and already-base64-encoded bytes so it can be
    /// emitted directly as an OpenAI `image_url` data URL or a Gemini
    /// `inline_data` part. Only user messages normally carry images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImagePart>>,
    #[serde(default)]
    pub hidden: bool,
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            hidden: false,
        }
    }

    pub fn hidden(role: Role, content: impl Into<String>) -> Self {
        let mut message = Self::new(role, content);
        message.hidden = true;
        message
    }

    pub fn with_display_content(mut self, content: impl Into<String>) -> Self {
        self.display_content = Some(content.into());
        self
    }

    pub fn with_images(mut self, images: Vec<ImagePart>) -> Self {
        self.images = if images.is_empty() { None } else { Some(images) };
        self
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            images: None,
            hidden: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String>;
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String>;
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        Ok(self
            .stream_chat(messages)
            .await?
            .filter_map(|item| async move {
                match item {
                    Ok(delta) if delta.is_empty() => None,
                    Ok(delta) => Some(Ok(ProviderStreamEvent::TextDelta(delta))),
                    Err(error) => Some(Err(error)),
                }
            })
            .boxed())
    }

    /// Called by the agent before each turn so the provider can prepare tool schemas.
    /// Default is a no-op for providers that don't support native function calling.
    fn prepare_tools(&self, _tools: &[Arc<dyn Tool>]) {}
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }
    /// Whether this specific invocation may run while the agent is in Plan
    /// mode. Defaults to read-only tools; write-capable tools can override to
    /// permit safe scopes (e.g. writing files under the plan directory).
    fn allowed_in_plan_mode(&self, _arguments: &str) -> bool {
        matches!(self.access(), ToolAccess::ReadOnly)
    }
    fn permission_scope(&self, _arguments: &str) -> String {
        "*".to_string()
    }
    async fn call(&self, arguments: &str) -> Result<String, String>;

    /// Execute the tool while optionally emitting events (e.g. sub-agent steps).
    ///
    /// The default implementation simply calls `call()` and emits no events.
    /// Tools that spawn sub-agents can override this to stream child events back
    /// to the parent harness.
    async fn call_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(SubTaskEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.call(arguments).await
    }

    /// Generate an OpenAI-compatible function schema for this tool.
    fn to_openai_function(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAccess {
    ReadOnly,
    Write,
}

pub mod commands;
pub mod mcp;
pub mod plan;
pub mod project;
pub mod providers;
pub mod skills;
pub mod tools;

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
    SwitchProvider {
        provider_type: String,
        model: String,
        api_key: Option<String>,
        base_url: Option<String>,
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
        duration_ms: u64,
    },
    PermissionRequest(PermissionRequest),
    PermissionsCleared,
    /// Lowercase provider name → whether a usable API key is configured.
    ProviderKeys(Vec<(String, bool)>),
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

/// Events emitted by a sub-agent spawned through the `task` tool.
///
/// These are forwarded from the child agent back to the parent harness so that
/// the TUI can render nested tool steps and streaming output inside the parent
/// tool card.
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
        duration_ms: u64,
    },
    GoalUpdated(Goal),
    /// The agent mode changed (e.g. via `plan_enter` / `plan_exit`). The TUI
    /// uses this to refresh its mode indicator live, mid-turn.
    ModeChanged(AgentMode),
    PermissionRequest(PermissionRequest),
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PermissionRule {
    tool: String,
    scope: String,
}

#[derive(Default)]
struct PermissionState {
    always: HashSet<PermissionRule>,
    pending: HashMap<String, oneshot::Sender<PermissionDecision>>,
}

pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    mode: Arc<std::sync::Mutex<AgentMode>>,
    /// In-memory runtime view of the active goal, used for the checklist.
    goal: Arc<std::sync::Mutex<Option<Goal>>>,
    permissions: std::sync::Mutex<PermissionState>,
    pub skills: Vec<skills::Skill>,
    goal_service: GoalService,
    thread_id: Arc<std::sync::Mutex<Option<String>>>,
}

/// Mutable bookkeeping threaded through a single turn's tool-dispatch rounds.
#[derive(Default)]
struct TurnState {
    token_usage: TokenUsage,
    /// The last tool `(name, arguments)` seen, used to bound consecutive repeats.
    previous_call: Option<(String, String)>,
    repeated_calls: usize,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        mode: AgentMode,
        goal_service: GoalService,
    ) -> Self {
        let skills = skills::discover_skills();
        let goal = Arc::new(std::sync::Mutex::new(None));
        let thread_id = Arc::new(std::sync::Mutex::new(None));
        let mode = Arc::new(std::sync::Mutex::new(mode));
        let context = goals::tools::GoalToolContext {
            thread_id: Arc::clone(&thread_id),
            goal_service: goal_service.clone(),
        };

        let mut tools = tools;
        tools.retain(|tool| !matches!(tool.name(), "goal_checklist" | "get_goal" | "create_goal" | "update_goal" | "plan_enter" | "plan_exit"));
        tools.push(Arc::new(goals::tools::GoalChecklistTool::new(
            context.clone(),
            Arc::clone(&goal),
        )));
        tools.push(Arc::new(goals::tools::GetGoalTool::new(context.clone())));
        tools.push(Arc::new(goals::tools::CreateGoalTool::new(context.clone())));
        tools.push(Arc::new(goals::tools::UpdateGoalTool::new(context.clone())));

        // Plan-mode workflow tools share the mode handle so they can flip it
        // in place; the agent emits a ModeChanged event after they run.
        let plan_context = plan::PlanToolContext::new(Arc::clone(&mode));
        tools.push(Arc::new(plan::PlanEnterTool::new(plan_context.clone())));
        tools.push(Arc::new(plan::PlanExitTool::new(plan_context)));

        Self {
            provider,
            tools,
            mode,
            goal,
            permissions: std::sync::Mutex::new(PermissionState::default()),
            skills,
            goal_service,
            thread_id,
        }
    }

    pub fn set_thread_id(&self, thread_id: impl Into<String>) {
        if let Ok(mut guard) = self.thread_id.lock() {
            *guard = Some(thread_id.into());
        }
    }

    pub fn clear_thread_id(&self) {
        if let Ok(mut guard) = self.thread_id.lock() {
            *guard = None;
        }
    }

    pub fn get_mode(&self) -> AgentMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_mode(&self, mode: AgentMode) {
        if let Ok(mut guard) = self.mode.lock() {
            *guard = mode;
        }
    }

    pub fn get_goal(&self) -> Option<Goal> {
        self.goal.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_goal(&self, goal: Goal) {
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = Some(goal);
    }

    pub fn restore_goal(&self, goal: Goal) {
        *self.goal.lock().unwrap_or_else(|error| error.into_inner()) = Some(goal);
    }

    pub fn clear_goal(&self) {
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn goal_can_complete(&self) -> bool {
        self.get_goal().is_some_and(|goal| goal.can_complete())
    }

    pub fn goal_service(&self) -> &GoalService {
        &self.goal_service
    }

    pub fn thread_id(&self) -> Option<String> {
        self.thread_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Append a hidden user message that asks the model to continue the active goal.
    pub fn inject_goal_continuation(&self, messages: &mut Vec<Message>) {
        if let Some(goal) = self.get_goal() {
            if goal.status == GoalStatus::Active {
                messages.push(Message::hidden(
                    Role::User,
                    goals::prompts::continuation_prompt(&goal),
                ));
            }
        }
    }

    /// Append a hidden user message that informs the model the goal objective changed.
    pub fn inject_objective_updated(&self, messages: &mut Vec<Message>) {
        if let Some(goal) = self.get_goal() {
            messages.push(Message::hidden(
                Role::User,
                goals::prompts::objective_updated_prompt(&goal),
            ));
        }
    }

    /// Append a hidden user message that informs the model the goal hit its budget.
    pub fn inject_budget_limit(&self, messages: &mut Vec<Message>) {
        if let Some(goal) = self.get_goal() {
            if goal.status == GoalStatus::BudgetLimited {
                messages.push(Message::hidden(
                    Role::User,
                    goals::prompts::budget_limit_prompt(&goal),
                ));
            }
        }
    }

    pub fn reply_permission(&self, request_id: &str, decision: PermissionDecision) -> bool {
        let sender = self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .remove(request_id);
        sender.is_some_and(|sender| sender.send(decision).is_ok())
    }

    pub fn reject_pending_permissions(&self) {
        let pending = std::mem::take(
            &mut self
                .permissions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending,
        );
        for (_, sender) in pending {
            let _ = sender.send(PermissionDecision::Reject);
        }
    }

    pub fn allowed_tools(&self) -> Vec<String> {
        let mut tools = self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .iter()
            .map(|rule| format!("{} {}", rule.tool, rule.scope))
            .collect::<Vec<_>>();
        tools.sort();
        tools
    }

    pub fn clear_allowed_tools(&self) {
        self.permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .clear();
    }

    /// Build a system prompt that includes tool definitions and skills index.
    fn build_system_prompt(&self) -> String {
        let mode = self.get_mode();
        let mut parts = vec![
            "You are neenee, an expert AI coding assistant with tool access.".to_string(),
            format!("Current mode: {:?}.", mode),
        ];

        parts.push(
            "Plan workflow: in Build mode, if a request is complex, spans multiple files, or would \
             benefit from designing first, call the plan_enter tool to switch to Plan mode. In Plan \
             mode, research with read-only tools, write the plan to .neenee/plans/<name>.md (the \
             only location you may write while planning), then call plan_exit to switch back to \
             Build mode and implement the plan. Do not enter Plan mode for simple tasks or when the \
             user wants immediate implementation."
                .to_string(),
        );

        if mode == AgentMode::Plan {
            parts.push(
                "You are currently in Plan mode. You may only use read-only tools, except that you \
                 may write files under .neenee/plans/. When the plan is written and finalized, call \
                 plan_exit to return to Build mode and implement it; do not implement edits while \
                 in Plan mode."
                    .to_string(),
            );
        }

        if let Some(goal) = self.get_goal() {
            parts.push(format!(
                "\nActive harness goal ({:?}):\n{}",
                goal.status, goal.objective
            ));
            if goal.status == GoalStatus::Active {
                if !goal.checklist.is_empty() {
                    parts.push(format!(
                        "Goal checklist:\n{}",
                        goal.checklist
                            .iter()
                            .map(|item| format!("- [{:?}] {}", item.status, item.content))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                parts.push(
                    "Work toward this goal across turns. Use get_goal to read the current goal, \
                     create_goal when the user asks for a new goal, update_goal to mark the goal \
                     complete or blocked, and goal_checklist to expose concrete progress items. \
                     Only when the objective is fully achieved, verified, and every checklist item \
                     is completed or cancelled, call update_goal with status \"complete\"."
                        .to_string(),
                );
            }
        }

        // Tool definitions
        if !self.tools.is_empty() {
            parts.push("\nAvailable tools:".to_string());
            for tool in &self.tools {
                parts.push(format!(
                    "  {} [{:?}]: {}\n    Parameters: {}",
                    tool.name(),
                    tool.access(),
                    tool.description(),
                    tool.parameters()
                ));
            }
            parts.push(
                "\nWhen you need to use a tool, output a JSON object in this exact format:\n\
                 {\"tool\": \"tool_name\", \"arguments\": {...}}\n\
                 Do not ask the user for permission — just call the tool."
                    .to_string(),
            );
        }

        // Skills index
        if !self.skills.is_empty() {
            parts.push(format!("\n{}", skills::build_skills_index(&self.skills)));
        }

        parts.join("\n")
    }

    /// Inject or update the system message in the message list.
    fn ensure_system_prompt(&self, messages: &mut Vec<Message>) {
        let prompt = self.build_system_prompt();
        if let Some(first) = messages.first_mut() {
            if first.role == Role::System {
                first.content = prompt;
                return;
            }
        }
        messages.insert(0, Message::new(Role::System, prompt));
    }

    /// Parse a tool call from assistant response text.
    /// Supports JSON format: {"tool": "name", "arguments": {...}}
    fn parse_tool_call(&self, text: &str) -> Option<ToolCall> {
        // Try to find a JSON object with "tool" key
        let trimmed = text.trim();
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(tool_name) = json.get("tool").and_then(|v| v.as_str()) {
                let args = json
                    .get("arguments")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                return Some(ToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4()),
                    name: tool_name.to_string(),
                    arguments: args,
                });
            }
        }
        None
    }

    /// Promote a text-based (fallback) tool call onto the preceding assistant
    /// message as a native `tool_calls` entry. This keeps the tool_call /
    /// tool_call_id pairing valid for OpenAI-compatible providers (which
    /// require every tool result to reference an assistant tool call), while
    /// non-native providers simply ignore the `tool_calls` field and keep using
    /// the message `content`.
    fn attach_fallback_tool_call(messages: &mut [Message], call: &ToolCall) {
        if let Some(last) = messages.last_mut() {
            if last.role == Role::Assistant && last.tool_calls.is_none() {
                last.tool_calls = Some(vec![call.clone()]);
            }
        }
    }

    pub async fn run(&self, messages: &mut Vec<Message>) -> Result<TurnOutcome, String> {
        self.run_with_events(messages, |event| {
            if let AgentEvent::PermissionRequest(request) = event {
                self.reply_permission(&request.id, PermissionDecision::Reject);
            }
        })
        .await
    }

    pub async fn run_with_events<F>(
        &self,
        messages: &mut Vec<Message>,
        mut on_event: F,
    ) -> Result<TurnOutcome, String>
    where
        F: FnMut(AgentEvent) + Send,
    {
        self.provider.prepare_tools(&self.tools);
        let turn_start = std::time::Instant::now();
        let mut state = TurnState::default();
        let mut tool_rounds = 0;

        loop {
            if tool_rounds >= MAX_TOOL_ROUNDS {
                return Err(format!(
                    "Agent stopped after {} tool rounds. Refine the goal or continue with /loop.",
                    MAX_TOOL_ROUNDS
                ));
            }

            remove_empty_assistant_messages(messages);
            self.ensure_system_prompt(messages);

            let response = self.provider.chat(messages.clone()).await?;
            if !valid_assistant_response(&response) {
                return Err("Provider returned an empty assistant response.".to_string());
            }
            state.token_usage.total_tokens += estimate_message_tokens(&response);
            messages.push(response.clone());

            // The model produced no text stream, so nothing was shown to the UI
            // that a fallback tool call would need to retract.
            if self
                .dispatch_tool_calls(&response, messages, &mut state, false, &mut on_event)
                .await?
            {
                tool_rounds += 1;
                continue;
            }

            return Ok(TurnOutcome {
                message: response,
                token_usage: state.token_usage,
                duration_ms: turn_start.elapsed().as_millis() as u64,
            });
        }
    }

    pub async fn run_streaming_with_events<F>(
        &self,
        messages: &mut Vec<Message>,
        mut on_event: F,
    ) -> Result<TurnOutcome, String>
    where
        F: FnMut(AgentEvent) + Send,
    {
        self.provider.prepare_tools(&self.tools);
        let turn_start = std::time::Instant::now();
        let mut state = TurnState::default();
        let mut tool_rounds = 0;

        loop {
            if tool_rounds >= MAX_TOOL_ROUNDS {
                return Err(format!(
                    "Agent stopped after {} tool rounds. Refine the goal or continue with /loop.",
                    MAX_TOOL_ROUNDS
                ));
            }

            remove_empty_assistant_messages(messages);
            self.ensure_system_prompt(messages);
            on_event(AgentEvent::ModelRequestStarted {
                tool_round: tool_rounds,
            });
            let mut stream = self.provider.stream_chat_events(messages.clone()).await?;
            let mut content = String::new();
            let mut reasoning_content = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut emitted_text = false;
            let mut emitted_reasoning = false;

            while let Some(event) = stream.next().await {
                match event? {
                    ProviderStreamEvent::TextDelta(delta) => {
                        content.push_str(&delta);
                        on_event(AgentEvent::AssistantDelta {
                            delta,
                            start: !emitted_text,
                        });
                        emitted_text = true;
                    }
                    ProviderStreamEvent::ReasoningDelta(delta) => {
                        reasoning_content.push_str(&delta);
                        on_event(AgentEvent::ReasoningDelta {
                            delta,
                            start: !emitted_reasoning,
                        });
                        emitted_reasoning = true;
                    }
                    ProviderStreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    } => {
                        while calls.len() <= index {
                            calls.push(ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                        }
                        let call = &mut calls[index];
                        if let Some(id) = id {
                            call.id.push_str(&id);
                        }
                        if let Some(name) = name {
                            call.name.push_str(&name);
                        }
                        call.arguments.push_str(&arguments);
                    }
                }
            }
            if emitted_text {
                on_event(AgentEvent::AssistantEnd(content.clone()));
            }
            if emitted_reasoning {
                on_event(AgentEvent::ReasoningEnd(reasoning_content.clone()));
            }

            calls.retain(|call| !call.name.is_empty());
            for call in &mut calls {
                if call.id.is_empty() {
                    call.id = format!("call_{}", uuid::Uuid::new_v4());
                }
            }
            let response = Message {
                role: Role::Assistant,
                content,
                display_content: None,
                reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
                tool_calls: (!calls.is_empty()).then_some(calls),
                tool_call_id: None,
                images: None,
                hidden: false,
            };
            if !valid_assistant_response(&response) {
                return Err("Provider returned an empty assistant response.".to_string());
            }
            state.token_usage.total_tokens += estimate_message_tokens(&response);
            messages.push(response.clone());

            // `emitted_text` means assistant text was already streamed to the
            // UI; a text-fallback tool call must then retract it via a discard.
            if self
                .dispatch_tool_calls(&response, messages, &mut state, emitted_text, &mut on_event)
                .await?
            {
                tool_rounds += 1;
                continue;
            }

            return Ok(TurnOutcome {
                message: response,
                token_usage: state.token_usage,
                duration_ms: turn_start.elapsed().as_millis() as u64,
            });
        }
    }

    /// Execute any tool calls carried by `response`, emitting events and
    /// appending tool results to `messages`. Shared by the streaming and
    /// non-streaming loops so the dispatch contract — repeated-call guard,
    /// up-front `ToolCall` events, concurrent execution with FIFO-ordered
    /// results, and goal/mode updates — lives in exactly one place.
    ///
    /// `streamed_text` is true when the response text was already streamed to
    /// the UI, so a recognised text-fallback tool call retracts it with an
    /// `AssistantDiscard`. Returns `true` when a tool round ran (the caller
    /// should loop again), `false` when the turn is complete.
    async fn dispatch_tool_calls<F>(
        &self,
        response: &Message,
        messages: &mut Vec<Message>,
        state: &mut TurnState,
        streamed_text: bool,
        on_event: &mut F,
    ) -> Result<bool, String>
    where
        F: FnMut(AgentEvent) + Send,
    {
        // Native tool calls (OpenAI-style function calling). An empty list is
        // treated as "no tool calls" so we fall through to the text fallback.
        if let Some(tool_calls) = response
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            for call in tool_calls {
                self.guard_repeated_call(call, &mut state.previous_call, &mut state.repeated_calls)?;
            }
            // Emit all ToolCall events up front.
            let call_ids: Vec<String> = tool_calls
                .iter()
                .map(|_| format!("call_{}", uuid::Uuid::new_v4()))
                .collect();
            for (call, id) in tool_calls.iter().zip(&call_ids) {
                on_event(AgentEvent::ToolCall {
                    id: id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
            }
            // Execute all tool calls concurrently; results arrive in input order.
            let results = self.execute_tools_concurrent(tool_calls, on_event).await;
            for ((call, id), (result, duration_ms)) in
                tool_calls.iter().zip(&call_ids).zip(results)
            {
                self.record_tool_result(call, id, &result, duration_ms, messages, state, on_event);
            }
            return Ok(true);
        }

        // Text-based fallback: any provider may emit a JSON tool call as text.
        if let Some(call) = self.parse_tool_call(&response.content) {
            if streamed_text {
                on_event(AgentEvent::AssistantDiscard);
            }
            self.guard_repeated_call(&call, &mut state.previous_call, &mut state.repeated_calls)?;
            Self::attach_fallback_tool_call(messages, &call);
            let call_id = format!("call_{}", uuid::Uuid::new_v4());
            on_event(AgentEvent::ToolCall {
                id: call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            let started = std::time::Instant::now();
            let result = self.execute_tool_evented(&call, on_event).await;
            let duration_ms = started.elapsed().as_millis() as u64;
            self.record_tool_result(&call, &call_id, &result, duration_ms, messages, state, on_event);
            return Ok(true);
        }

        Ok(false)
    }

    /// Account for, surface, and persist a single tool result. The argument
    /// count reflects the per-result state it must thread; grouping it further
    /// would only move the noise to the call sites.
    #[allow(clippy::too_many_arguments)]
    fn record_tool_result<F>(
        &self,
        call: &ToolCall,
        call_id: &str,
        result: &str,
        duration_ms: u64,
        messages: &mut Vec<Message>,
        state: &mut TurnState,
        on_event: &mut F,
    ) where
        F: FnMut(AgentEvent) + Send,
    {
        state.token_usage.total_tokens += estimate_string_tokens(result);
        self.emit_goal_update(call, on_event);
        self.emit_mode_change(call, on_event);
        on_event(AgentEvent::ToolResult {
            id: call_id.to_string(),
            name: call.name.clone(),
            output: result.to_string(),
            duration_ms,
        });
        messages.push(Message::tool_result(
            call,
            format!("[{} result]:\n{}", call.name, result),
        ));
    }

    fn guard_repeated_call(
        &self,
        call: &ToolCall,
        previous_call: &mut Option<(String, String)>,
        repeated_calls: &mut usize,
    ) -> Result<(), String> {
        let signature = (call.name.clone(), call.arguments.clone());
        if previous_call.as_ref() == Some(&signature) {
            *repeated_calls += 1;
        } else {
            *previous_call = Some(signature);
            *repeated_calls = 1;
        }

        if *repeated_calls > MAX_REPEATED_TOOL_CALLS {
            return Err(format!(
                "Agent stopped after repeating the same '{}' tool call {} times.",
                call.name, MAX_REPEATED_TOOL_CALLS
            ));
        }
        Ok(())
    }

    fn emit_goal_update<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if call.name == "goal_checklist" {
            if let Some(goal) = self.get_goal() {
                on_event(AgentEvent::GoalUpdated(goal));
            }
        }
    }

    /// Notify the harness that the agent mode changed via `plan_enter` /
    /// `plan_exit`. The tools mutate the shared mode cell themselves; this
    /// only emits the live `ModeChanged` event so the TUI can refresh.
    fn emit_mode_change<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if call.name == "plan_enter" || call.name == "plan_exit" {
            on_event(AgentEvent::ModeChanged(self.get_mode()));
        }
    }

    async fn execute_tool(
        &self,
        call: &ToolCall,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> String {
        let tool = match self.tools.iter().find(|t| t.name() == call.name) {
            Some(t) => t,
            None => return format!("Error: Tool '{}' not found", call.name),
        };

        if self.get_mode() == AgentMode::Plan && !tool.allowed_in_plan_mode(&call.arguments) {
            return format!(
                "[Plan mode] Tool '{}' is blocked. Switch to Build mode to execute it.",
                call.name
            );
        }

        if tool.access() == ToolAccess::Write {
            let scope = tool.permission_scope(&call.arguments);
            let rule = PermissionRule {
                tool: tool.name().to_string(),
                scope: scope.clone(),
            };
            let always_allowed = self
                .permissions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .always
                .contains(&rule);
            if !always_allowed {
                let request = PermissionRequest {
                    id: format!("permission_{}", uuid::Uuid::new_v4()),
                    tool: tool.name().to_string(),
                    description: tool.description().to_string(),
                    arguments: call.arguments.clone(),
                    scope,
                };
                let (sender, receiver) = oneshot::channel();
                self.permissions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .insert(request.id.clone(), sender);
                let _ = event_tx.send(AgentEvent::PermissionRequest(request.clone()));

                match receiver.await.unwrap_or(PermissionDecision::Reject) {
                    PermissionDecision::Once => {}
                    PermissionDecision::Always => {
                        self.permissions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .always
                            .insert(rule);
                    }
                    PermissionDecision::Reject => {
                        return format!(
                            "Permission denied for tool '{}'. Do not retry the same call.",
                            tool.name()
                        );
                    }
                }
            }
        }

        let parent_call_id = call.id.clone();
        match tool
            .call_with_events(
                &call.id,
                &call.arguments,
                Box::new(|event| {
                    let _ = event_tx.send(AgentEvent::SubTask {
                        parent_call_id: parent_call_id.clone(),
                        event,
                    });
                }),
            )
            .await
        {
            Ok(output) => output,
            Err(err) => format!("Error executing {}: {}", call.name, err),
        }
    }

    /// Single-call wrapper that forwards channel events to a mutable callback.
    /// Used by text-fallback paths (one tool call at a time).
    async fn execute_tool_evented<F>(&self, call: &ToolCall, on_event: &mut F) -> String
    where
        F: FnMut(AgentEvent) + Send,
    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fut = self.execute_tool(call, &tx);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    if let Some(event) = event {
                        on_event(event);
                    }
                }
                result = &mut fut => {
                    while let Ok(event) = rx.try_recv() {
                        on_event(event);
                    }
                    return result;
                }
            }
        }
    }

    /// Execute multiple tool calls concurrently, forwarding interleaved events
    /// to the callback in real time. Returns `(result, duration_ms)` pairs in
    /// the same order as the input calls.
    async fn execute_tools_concurrent<F>(
        &self,
        calls: &[ToolCall],
        on_event: &mut F,
    ) -> Vec<(String, u64)>
    where
        F: FnMut(AgentEvent) + Send,
    {
        let (tx, mut rx) = mpsc::unbounded_channel();

        let futs: Vec<_> = calls
            .iter()
            .map(|call| {
                let tx = tx.clone();
                async move {
                    let started = std::time::Instant::now();
                    let result = self.execute_tool(call, &tx).await;
                    (result, started.elapsed().as_millis() as u64)
                }
            })
            .collect();

        let all = join_all(futs);
        tokio::pin!(all);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    if let Some(event) = event {
                        on_event(event);
                    }
                }
                results = &mut all => {
                    while let Ok(event) = rx.try_recv() {
                        on_event(event);
                    }
                    return results;
                }
            }
        }
    }
}

fn valid_assistant_response(message: &Message) -> bool {
    !message.content.is_empty()
        || message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
}

fn remove_empty_assistant_messages(messages: &mut Vec<Message>) {
    messages.retain(|message| message.role != Role::Assistant || valid_assistant_response(message));
}

fn estimate_message_tokens(message: &Message) -> i64 {
    let text_len = message.content.len()
        + message
            .reasoning_content
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0);
    let tool_text: usize = message
        .tool_calls
        .as_ref()
        .map(|calls| calls.iter().map(|c| c.name.len() + c.arguments.len()).sum())
        .unwrap_or(0);
    estimate_string_tokens_len(text_len + tool_text)
}

fn estimate_string_tokens(s: &str) -> i64 {
    estimate_string_tokens_len(s.len())
}

fn estimate_string_tokens_len(len: usize) -> i64 {
    // Rough heuristic: ~4 characters per token for English text.
    // Providers that report real usage should override this estimate.
    (len / 4).max(1) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestProvider;
    struct PermissionTestProvider(AtomicUsize);
    struct StreamingToolProvider(AtomicUsize);
    struct WriteTestTool;
    struct StreamingReadTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Provider for TestProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Ok(Message::new(Role::Assistant, "done"))
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }
    }

    #[async_trait]
    impl Provider for PermissionTestProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Message {
                    role: Role::Assistant,
                    content: String::new(),
                    display_content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call".to_string(),
                        name: "write_test".to_string(),
                        arguments: "{}".to_string(),
                    }]),
                    tool_call_id: None,
                    images: None,
                    hidden: false,
                })
            } else {
                Ok(Message::new(Role::Assistant, "done"))
            }
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }
    }

    #[async_trait]
    impl Provider for StreamingToolProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("non-streaming path should not be used".to_string())
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }

        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
            let events = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![
                    Ok(ProviderStreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_string()),
                        name: Some("stream_".to_string()),
                        arguments: "{\"value\":".to_string(),
                    }),
                    Ok(ProviderStreamEvent::ToolCallDelta {
                        index: 0,
                        id: None,
                        name: Some("read".to_string()),
                        arguments: "1}".to_string(),
                    }),
                ]
            } else {
                vec![
                    Ok(ProviderStreamEvent::TextDelta("do".to_string())),
                    Ok(ProviderStreamEvent::TextDelta("ne".to_string())),
                ]
            };
            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[async_trait]
    impl Tool for WriteTestTool {
        fn name(&self) -> &str {
            "write_test"
        }

        fn description(&self) -> &str {
            "test write tool"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("should not run".to_string())
        }
    }

    #[async_trait]
    impl Tool for StreamingReadTool {
        fn name(&self) -> &str {
            "stream_read"
        }

        fn description(&self) -> &str {
            "streaming test tool"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn access(&self) -> ToolAccess {
            ToolAccess::ReadOnly
        }

        async fn call(&self, arguments: &str) -> Result<String, String> {
            assert_eq!(arguments, "{\"value\":1}");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("read".to_string())
        }
    }

    fn test_goal_service() -> GoalService {
        GoalService::new(GoalStore::open_in_memory_blocking().expect("in-memory goal store"))
    }

    fn agent() -> Agent {
        Agent::new(
            Arc::new(TestProvider),
            Vec::new(),
            AgentMode::Build,
            test_goal_service(),
        )
    }

    fn active_goal(objective: &str) -> Goal {
        Goal {
            objective: objective.to_string(),
            status: GoalStatus::Active,
            checklist: Vec::new(),
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
        }
    }

    #[test]
    fn goal_is_injected_into_system_prompt() {
        let agent = agent();
        agent.set_goal(active_goal("ship the harness"));

        let prompt = agent.build_system_prompt();

        assert!(prompt.contains("ship the harness"));
        assert!(prompt.contains("update_goal"));
    }

    #[test]
    fn retry_metadata_is_not_exposed_as_public_error_text() {
        let encoded = retryable_error("rate limited", Some(500));
        assert_eq!(public_error_message(&encoded), "rate limited");
        assert_eq!(public_error_message("plain"), "plain");
    }

    #[test]
    fn goal_lifecycle_is_explicit() {
        let agent = agent();
        agent.set_goal(active_goal("verify behavior"));
        assert_eq!(agent.get_goal().unwrap().status, GoalStatus::Active);

        let mut completed = active_goal("verify behavior");
        completed.status = GoalStatus::Complete;
        agent.set_goal(completed);
        assert_eq!(agent.get_goal().unwrap().status, GoalStatus::Complete);

        agent.clear_goal();
        assert_eq!(agent.get_goal(), None);
    }

    #[tokio::test]
    async fn goal_checklist_controls_completion_readiness() {
        let agent = agent();
        agent.set_goal(active_goal("ship verified work"));
        let tool = agent
            .tools
            .iter()
            .find(|tool| tool.name() == "goal_checklist")
            .unwrap();

        tool.call(
            r#"{"items":[
                {"content":"implement","status":"completed"},
                {"content":"verify","status":"in_progress"}
            ]}"#,
        )
        .await
        .unwrap();
        assert!(!agent.goal_can_complete());

        tool.call(
            r#"{"items":[
                {"content":"implement","status":"completed"},
                {"content":"verify","status":"completed"}
            ]}"#,
        )
        .await
        .unwrap();
        assert!(agent.goal_can_complete());
    }

    #[tokio::test]
    async fn goal_checklist_rejects_multiple_in_progress_items() {
        let agent = agent();
        agent.set_goal(active_goal("track work"));
        let tool = agent
            .tools
            .iter()
            .find(|tool| tool.name() == "goal_checklist")
            .unwrap();

        let error = tool
            .call(
                r#"{"items":[
                    {"content":"one","status":"in_progress"},
                    {"content":"two","status":"in_progress"}
                ]}"#,
            )
            .await
            .unwrap_err();

        assert!(error.contains("At most one"));
    }

    #[tokio::test]
    async fn goal_checklist_cannot_be_silently_cleared() {
        let agent = agent();
        agent.set_goal(active_goal("track work"));
        let tool = agent
            .tools
            .iter()
            .find(|tool| tool.name() == "goal_checklist")
            .unwrap();
        tool.call(r#"{"items":[{"content":"verify","status":"pending"}]}"#)
            .await
            .unwrap();

        let error = tool.call(r#"{"items":[]}"#).await.unwrap_err();

        assert!(error.contains("cannot be cleared"));
        assert!(!agent.goal_can_complete());
    }

    #[test]
    fn goal_checklist_updates_emit_harness_state() {
        let agent = agent();
        agent.set_goal(active_goal("track"));
        let call = ToolCall {
            id: "call".to_string(),
            name: "goal_checklist".to_string(),
            arguments: "{}".to_string(),
        };
        let mut events = Vec::new();

        agent.emit_goal_update(&call, &mut |event| events.push(event));

        assert!(matches!(
            events.as_slice(),
            [AgentEvent::GoalUpdated(Goal { objective, .. })] if objective == "track"
        ));
    }

    #[tokio::test]
    async fn streaming_tool_deltas_are_reassembled_and_executed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            Arc::new(StreamingToolProvider(AtomicUsize::new(0))),
            vec![Arc::new(StreamingReadTool(calls.clone()))],
            AgentMode::Build,
            test_goal_service(),
        );
        let mut messages = vec![Message::new(Role::User, "run")];
        let mut events = Vec::new();

        let response = agent
            .run_streaming_with_events(&mut messages, |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(response.message.content, "done");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let model_rounds = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelRequestStarted { tool_round } => Some(*tool_round),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(model_rounds, vec![0, 1]);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCall { name, arguments, .. }
                if name == "stream_read" && arguments == "{\"value\":1}"
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::AssistantEnd(content)) if content == "done"
        ));
    }

    #[test]
    fn repeated_tool_calls_are_bounded() {
        let agent = agent();
        let call = ToolCall {
            id: "call".to_string(),
            name: "read_file".to_string(),
            arguments: "{\"path\":\"README.md\"}".to_string(),
        };
        let mut previous = None;
        let mut repeats = 0;

        for _ in 0..MAX_REPEATED_TOOL_CALLS {
            assert!(agent
                .guard_repeated_call(&call, &mut previous, &mut repeats)
                .is_ok());
        }
        assert!(agent
            .guard_repeated_call(&call, &mut previous, &mut repeats)
            .is_err());
    }

    #[tokio::test]
    async fn plan_mode_blocks_tools_unless_explicitly_read_only() {
        let agent = Agent::new(
            Arc::new(TestProvider),
            vec![Arc::new(WriteTestTool)],
            AgentMode::Plan,
            test_goal_service(),
        );
        let call = ToolCall {
            id: "call".to_string(),
            name: "write_test".to_string(),
            arguments: "{}".to_string(),
        };

        assert!(agent
            .execute_tool_evented(&call, &mut |_| {})
            .await
            .contains("[Plan mode]"));
    }

    #[tokio::test]
    async fn write_tool_waits_for_permission_and_always_is_cached() {
        let agent = Arc::new(Agent::new(
            Arc::new(TestProvider),
            vec![Arc::new(WriteTestTool)],
            AgentMode::Build,
            test_goal_service(),
        ));
        let call = ToolCall {
            id: "call".to_string(),
            name: "write_test".to_string(),
            arguments: "{}".to_string(),
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let task_agent = agent.clone();
        let task_call = call.clone();
        let task = tokio::spawn(async move {
            task_agent
                .execute_tool_evented(&task_call, &mut |event| {
                    let _ = event_tx.send(event);
                })
                .await
        });

        let request = match event_rx.recv().await.unwrap() {
            AgentEvent::PermissionRequest(request) => request,
            event => panic!("unexpected event: {:?}", event),
        };
        assert!(!task.is_finished());
        assert!(agent.reply_permission(&request.id, PermissionDecision::Always));
        assert_eq!(task.await.unwrap(), "should not run");
        assert_eq!(agent.allowed_tools(), vec!["write_test *".to_string()]);

        let mut prompted_again = false;
        let output = agent
            .execute_tool_evented(&call, &mut |event| {
                if matches!(event, AgentEvent::PermissionRequest(_)) {
                    prompted_again = true;
                }
            })
            .await;
        assert_eq!(output, "should not run");
        assert!(!prompted_again);
    }

    #[tokio::test]
    async fn rejected_permission_does_not_execute_tool() {
        let agent = Arc::new(Agent::new(
            Arc::new(TestProvider),
            vec![Arc::new(WriteTestTool)],
            AgentMode::Build,
            test_goal_service(),
        ));
        let call = ToolCall {
            id: "call".to_string(),
            name: "write_test".to_string(),
            arguments: "{}".to_string(),
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let task_agent = agent.clone();
        let task = tokio::spawn(async move {
            task_agent
                .execute_tool_evented(&call, &mut |event| {
                    let _ = event_tx.send(event);
                })
                .await
        });

        let request = match event_rx.recv().await.unwrap() {
            AgentEvent::PermissionRequest(request) => request,
            event => panic!("unexpected event: {:?}", event),
        };
        assert!(agent.reply_permission(&request.id, PermissionDecision::Reject));
        assert!(task.await.unwrap().contains("Permission denied"));
    }

    #[tokio::test]
    async fn headless_run_rejects_write_tools_without_hanging() {
        let goal_service = GoalService::new(
            GoalStore::open_in_memory().await.expect("in-memory goal store"),
        );
        let agent = Agent::new(
            Arc::new(PermissionTestProvider(AtomicUsize::new(0))),
            vec![Arc::new(WriteTestTool)],
            AgentMode::Build,
            goal_service,
        );
        let mut messages = vec![Message::new(Role::User, "write something")];

        let outcome = agent.run(&mut messages).await.unwrap();

        assert_eq!(outcome.message.content, "done");
        assert!(messages
            .iter()
            .any(|message| message.content.contains("Permission denied")));
    }

    // ---- Golden-transcript harness ----------------------------------------
    //
    // `ScriptedProvider` replays a fixed list of streamed events — one script
    // per model round — so a whole agent turn runs deterministically and its
    // emitted `AgentEvent` stream can be asserted as a stable golden
    // transcript. This pins the loop's externally-visible contract (tool-call
    // ordering, native vs text-fallback dispatch, concurrent result ordering,
    // the repeated-call guard, and permission gating) independently of any real
    // provider, so the refactors that follow can lean on it as a safety net.

    /// A model round that streams a single chunk of assistant text.
    fn text_round(text: &str) -> Vec<ProviderStreamEvent> {
        vec![ProviderStreamEvent::TextDelta(text.to_string())]
    }

    /// A model round that streams native tool calls as `(id, name, arguments)`.
    fn tool_round(calls: &[(&str, &str, &str)]) -> Vec<ProviderStreamEvent> {
        calls
            .iter()
            .enumerate()
            .map(|(index, (id, name, arguments))| ProviderStreamEvent::ToolCallDelta {
                index,
                id: Some(id.to_string()),
                name: Some(name.to_string()),
                arguments: arguments.to_string(),
            })
            .collect()
    }

    struct ScriptedProvider {
        rounds: std::sync::Mutex<std::collections::VecDeque<Vec<ProviderStreamEvent>>>,
    }

    impl ScriptedProvider {
        fn new(rounds: Vec<Vec<ProviderStreamEvent>>) -> Self {
            Self {
                rounds: std::sync::Mutex::new(rounds.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("scripted provider is streaming-only".to_string())
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }

        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
            // A turn that runs past its script gets a terminal "done" so the
            // loop exits rather than hanging on a missing round.
            let round = self
                .rounds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(|| text_round("done"));
            Ok(Box::pin(stream::iter(round.into_iter().map(Ok))))
        }
    }

    /// A tool that records every invocation's arguments and returns canned
    /// output, with a configurable access level for permission tests.
    struct RecordingTool {
        name: &'static str,
        access: ToolAccess,
        output: String,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingTool {
        fn read(name: &'static str, output: &str) -> Self {
            Self {
                name,
                access: ToolAccess::ReadOnly,
                output: output.to_string(),
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn write(name: &'static str, output: &str) -> Self {
            Self {
                access: ToolAccess::Write,
                ..Self::read(name, output)
            }
        }

        fn calls_handle(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
            Arc::clone(&self.calls)
        }
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "recording test tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn access(&self) -> ToolAccess {
            self.access
        }
        async fn call(&self, arguments: &str) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(arguments.to_string());
            Ok(self.output.clone())
        }
    }

    /// Normalise an event stream into a stable, assertable transcript by
    /// dropping non-deterministic fields (generated call ids and durations).
    fn transcript(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                AgentEvent::ModelRequestStarted { tool_round } => {
                    format!("model-request round={tool_round}")
                }
                AgentEvent::AssistantDelta { delta, start } => {
                    format!("assistant-delta start={start} {delta:?}")
                }
                AgentEvent::AssistantEnd(content) => format!("assistant-end {content:?}"),
                AgentEvent::AssistantDiscard => "assistant-discard".to_string(),
                AgentEvent::ReasoningDelta { delta, start } => {
                    format!("reasoning-delta start={start} {delta:?}")
                }
                AgentEvent::ReasoningEnd(content) => format!("reasoning-end {content:?}"),
                AgentEvent::ToolCall { name, arguments, .. } => {
                    format!("tool-call {name} {arguments}")
                }
                AgentEvent::ToolResult { name, output, .. } => {
                    format!("tool-result {name} {output:?}")
                }
                AgentEvent::GoalUpdated(_) => "goal-updated".to_string(),
                AgentEvent::ModeChanged(mode) => format!("mode-changed {mode:?}"),
                AgentEvent::PermissionRequest(request) => {
                    format!("permission-request {} {}", request.tool, request.scope)
                }
                AgentEvent::SubTask { .. } => "subtask".to_string(),
            })
            .collect()
    }

    /// Drive one full turn, auto-answering any permission prompt with `decision`
    /// so write-capable tools don't deadlock the loop.
    async fn run_golden_turn(
        agent: &Agent,
        prompt: &str,
        decision: PermissionDecision,
    ) -> (Vec<AgentEvent>, Result<TurnOutcome, String>) {
        let mut messages = vec![Message::new(Role::User, prompt)];
        let mut events = Vec::new();
        let outcome = agent
            .run_streaming_with_events(&mut messages, |event| {
                if let AgentEvent::PermissionRequest(request) = &event {
                    agent.reply_permission(&request.id, decision);
                }
                events.push(event);
            })
            .await;
        (events, outcome)
    }

    #[tokio::test]
    async fn golden_native_tool_round_then_final_text() {
        let agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![
                tool_round(&[("c1", "alpha", "{\"k\":1}"), ("c2", "beta", "{\"k\":2}")]),
                text_round("all done"),
            ])),
            vec![
                Arc::new(RecordingTool::read("alpha", "A-out")),
                Arc::new(RecordingTool::read("beta", "B-out")),
            ],
            AgentMode::Build,
            test_goal_service(),
        );

        let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

        assert_eq!(outcome.unwrap().message.content, "all done");
        // Calls are announced up front, then results land in input (FIFO) order
        // regardless of concurrent execution.
        assert_eq!(
            transcript(&events),
            vec![
                "model-request round=0",
                "tool-call alpha {\"k\":1}",
                "tool-call beta {\"k\":2}",
                "tool-result alpha \"A-out\"",
                "tool-result beta \"B-out\"",
                "model-request round=1",
                "assistant-delta start=true \"all done\"",
                "assistant-end \"all done\"",
            ]
        );
    }

    #[tokio::test]
    async fn golden_text_fallback_tool_call_is_discarded_then_dispatched() {
        let agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![
                text_round("{\"tool\":\"alpha\",\"arguments\":{\"k\":1}}"),
                text_round("finished"),
            ])),
            vec![Arc::new(RecordingTool::read("alpha", "A-out"))],
            AgentMode::Build,
            test_goal_service(),
        );

        let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

        assert_eq!(outcome.unwrap().message.content, "finished");
        // The streamed JSON is shown, then discarded once recognised as a tool
        // call, so the UI never leaves raw tool JSON on screen.
        assert_eq!(
            transcript(&events),
            vec![
                "model-request round=0",
                "assistant-delta start=true \"{\\\"tool\\\":\\\"alpha\\\",\\\"arguments\\\":{\\\"k\\\":1}}\"",
                "assistant-end \"{\\\"tool\\\":\\\"alpha\\\",\\\"arguments\\\":{\\\"k\\\":1}}\"",
                "assistant-discard",
                "tool-call alpha {\"k\":1}",
                "tool-result alpha \"A-out\"",
                "model-request round=1",
                "assistant-delta start=true \"finished\"",
                "assistant-end \"finished\"",
            ]
        );
    }

    #[tokio::test]
    async fn golden_repeated_identical_tool_calls_abort_the_turn() {
        let tool = RecordingTool::read("alpha", "A-out");
        let calls = tool.calls_handle();
        // Four identical rounds: the guard trips on the fourth.
        let identical = || tool_round(&[("c", "alpha", "{}")]);
        let agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![
                identical(),
                identical(),
                identical(),
                identical(),
            ])),
            vec![Arc::new(tool)],
            AgentMode::Build,
            test_goal_service(),
        );

        let (_events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

        assert!(outcome.unwrap_err().contains("repeating the same"));
        // The first MAX_REPEATED_TOOL_CALLS calls run; the fourth is blocked.
        assert_eq!(
            calls.lock().unwrap().len(),
            MAX_REPEATED_TOOL_CALLS,
            "guard must stop before executing the repeat"
        );
    }

    #[tokio::test]
    async fn golden_rejected_write_tool_is_gated_and_loop_continues() {
        let tool = RecordingTool::write("writer", "WROTE");
        let calls = tool.calls_handle();
        let agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![
                tool_round(&[("c1", "writer", "{\"path\":\"x\"}")]),
                text_round("stopped"),
            ])),
            vec![Arc::new(tool)],
            AgentMode::Build,
            test_goal_service(),
        );

        let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

        assert_eq!(outcome.unwrap().message.content, "stopped");
        assert!(
            calls.lock().unwrap().is_empty(),
            "rejected write tool must not execute"
        );
        let lines = transcript(&events);
        assert!(lines.iter().any(|line| line == "permission-request writer *"));
        assert!(lines
            .iter()
            .any(|line| line.starts_with("tool-result writer") && line.contains("Permission denied")));
    }

    #[tokio::test]
    async fn golden_reasoning_precedes_text_in_the_same_round() {
        let agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![vec![
                ProviderStreamEvent::ReasoningDelta("think".to_string()),
                ProviderStreamEvent::TextDelta("answer".to_string()),
            ]])),
            Vec::new(),
            AgentMode::Build,
            test_goal_service(),
        );

        let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

        assert_eq!(outcome.unwrap().message.content, "answer");
        // Deltas surface in stream-arrival order (reasoning first here), but the
        // round closes with AssistantEnd before ReasoningEnd.
        assert_eq!(
            transcript(&events),
            vec![
                "model-request round=0",
                "reasoning-delta start=true \"think\"",
                "assistant-delta start=true \"answer\"",
                "assistant-end \"answer\"",
                "reasoning-end \"think\"",
            ]
        );
    }
}
