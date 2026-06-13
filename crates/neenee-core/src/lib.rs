pub use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::oneshot;

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
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            display_content: None,
            tool_calls: None,
            tool_call_id: None,
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

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            display_content: None,
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
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
    fn permission_scope(&self, _arguments: &str) -> String {
        "*".to_string()
    }
    async fn call(&self, arguments: &str) -> Result<String, String>;

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
pub mod providers;
pub mod skills;
pub mod tools;

#[derive(Debug)]
pub enum AgentRequest {
    Chat(String),
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
}

#[derive(Debug)]
pub enum AgentResponse {
    Text(String),
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        output: String,
    },
    PermissionRequest(PermissionRequest),
    PermissionsCleared,
    /// Lowercase provider name → whether a usable API key is configured.
    ProviderKeys(Vec<(String, bool)>),
    ConversationCleared,
    ConversationReplaced(Vec<Message>),
    Compacted {
        archived_messages: usize,
        before_chars: usize,
        after_chars: usize,
    },
    HarnessState(HarnessSnapshot),
    GoalUpdated(Goal),
    RetryScheduled {
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        message: String,
    },
    Activity(String),
    StreamStart,
    StreamDelta(String),
    StreamEnd(String),
    StreamDiscard,
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
pub enum GoalStatus {
    Active,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub checklist: Vec<GoalChecklistItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChecklistStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalChecklistItem {
    pub content: String,
    pub status: GoalChecklistStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSnapshot {
    pub mode: AgentMode,
    pub goal: Option<Goal>,
    pub loop_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    ModelRequestStarted { tool_round: usize },
    AssistantDelta { delta: String, start: bool },
    AssistantEnd(String),
    AssistantDiscard,
    ToolCall { name: String, arguments: String },
    ToolResult { name: String, output: String },
    GoalUpdated(Goal),
    PermissionRequest(PermissionRequest),
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
    mode: std::sync::Mutex<AgentMode>,
    goal: Arc<std::sync::Mutex<Option<Goal>>>,
    permissions: std::sync::Mutex<PermissionState>,
    pub skills: Vec<skills::Skill>,
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>, mode: AgentMode) -> Self {
        let skills = skills::discover_skills();
        let goal = Arc::new(std::sync::Mutex::new(None));
        let mut tools = tools;
        tools.retain(|tool| tool.name() != "goal_checklist");
        tools.push(Arc::new(tools::GoalChecklistTool::new(goal.clone())));
        Self {
            provider,
            tools,
            mode: std::sync::Mutex::new(mode),
            goal,
            permissions: std::sync::Mutex::new(PermissionState::default()),
            skills,
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

    pub fn set_goal(&self, objective: impl Into<String>) -> Goal {
        let goal = Goal {
            objective: objective.into(),
            status: GoalStatus::Active,
            checklist: Vec::new(),
        };
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = Some(goal.clone());
        goal
    }

    pub fn restore_goal(&self, goal: Goal) {
        *self.goal.lock().unwrap_or_else(|error| error.into_inner()) = Some(goal);
    }

    pub fn complete_goal(&self) -> Option<Goal> {
        let mut guard = self.goal.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(goal) = guard.as_mut() {
            goal.status = GoalStatus::Completed;
        }
        guard.clone()
    }

    pub fn clear_goal(&self) {
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn goal_can_complete(&self) -> bool {
        self.get_goal().is_some_and(|goal| {
            goal.checklist.is_empty()
                || goal.checklist.iter().all(|item| {
                    matches!(
                        item.status,
                        GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                    )
                })
        })
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

        if mode == AgentMode::Plan {
            parts.push(
                "In Plan mode, you may only use tools marked ReadOnly. Write-capable and \
                 unclassified tools are blocked and will return an error."
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
                parts.push(format!(
                    "Work toward this goal across turns. Only when the objective is fully \
                     achieved, verified, and every checklist item is completed or cancelled, \
                     include {} on its own line in the final response. Use goal_checklist to \
                     create and update concrete progress items.",
                    GOAL_COMPLETE_MARKER
                ));
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

    pub async fn run(&self, messages: &mut Vec<Message>) -> Result<Message, String> {
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
    ) -> Result<Message, String>
    where
        F: FnMut(AgentEvent),
    {
        self.provider.prepare_tools(&self.tools);
        let mut tool_rounds = 0;
        let mut previous_call: Option<(String, String)> = None;
        let mut repeated_calls = 0;

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
            messages.push(response.clone());

            // Check for native tool calls (OpenAI function calling)
            if let Some(tool_calls) = &response.tool_calls {
                if !tool_calls.is_empty() {
                    for call in tool_calls {
                        self.guard_repeated_call(call, &mut previous_call, &mut repeated_calls)?;
                        on_event(AgentEvent::ToolCall {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        });
                        let result = self.execute_tool(call, &mut on_event).await;
                        self.emit_goal_update(call, &mut on_event);
                        on_event(AgentEvent::ToolResult {
                            name: call.name.clone(),
                            output: result.clone(),
                        });
                        messages.push(Message::tool_result(
                            call,
                            format!("[{} result]:\n{}", call.name, result),
                        ));
                    }
                    tool_rounds += 1;
                    continue;
                }
            }

            // Check for text-based tool calls (universal fallback for all providers)
            if let Some(call) = self.parse_tool_call(&response.content) {
                self.guard_repeated_call(&call, &mut previous_call, &mut repeated_calls)?;
                on_event(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = self.execute_tool(&call, &mut on_event).await;
                self.emit_goal_update(&call, &mut on_event);
                on_event(AgentEvent::ToolResult {
                    name: call.name.clone(),
                    output: result.clone(),
                });
                messages.push(Message::tool_result(
                    &call,
                    format!("[{} result]:\n{}", call.name, result),
                ));
                tool_rounds += 1;
                continue;
            }

            return Ok(response);
        }
    }

    pub async fn run_streaming_with_events<F>(
        &self,
        messages: &mut Vec<Message>,
        mut on_event: F,
    ) -> Result<Message, String>
    where
        F: FnMut(AgentEvent),
    {
        self.provider.prepare_tools(&self.tools);
        let mut tool_rounds = 0;
        let mut previous_call: Option<(String, String)> = None;
        let mut repeated_calls = 0;

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
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut emitted_text = false;

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
                tool_calls: (!calls.is_empty()).then_some(calls),
                tool_call_id: None,
                hidden: false,
            };
            if !valid_assistant_response(&response) {
                return Err("Provider returned an empty assistant response.".to_string());
            }
            messages.push(response.clone());

            if let Some(tool_calls) = &response.tool_calls {
                for call in tool_calls {
                    self.guard_repeated_call(call, &mut previous_call, &mut repeated_calls)?;
                    on_event(AgentEvent::ToolCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                    let result = self.execute_tool(call, &mut on_event).await;
                    self.emit_goal_update(call, &mut on_event);
                    on_event(AgentEvent::ToolResult {
                        name: call.name.clone(),
                        output: result.clone(),
                    });
                    messages.push(Message::tool_result(
                        call,
                        format!("[{} result]:\n{}", call.name, result),
                    ));
                }
                tool_rounds += 1;
                continue;
            }

            if let Some(call) = self.parse_tool_call(&response.content) {
                if emitted_text {
                    on_event(AgentEvent::AssistantDiscard);
                }
                self.guard_repeated_call(&call, &mut previous_call, &mut repeated_calls)?;
                on_event(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = self.execute_tool(&call, &mut on_event).await;
                self.emit_goal_update(&call, &mut on_event);
                on_event(AgentEvent::ToolResult {
                    name: call.name.clone(),
                    output: result.clone(),
                });
                messages.push(Message::tool_result(
                    &call,
                    format!("[{} result]:\n{}", call.name, result),
                ));
                tool_rounds += 1;
                continue;
            }

            return Ok(response);
        }
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
        F: FnMut(AgentEvent),
    {
        if call.name == "goal_checklist" {
            if let Some(goal) = self.get_goal() {
                on_event(AgentEvent::GoalUpdated(goal));
            }
        }
    }

    async fn execute_tool<F>(&self, call: &ToolCall, on_event: &mut F) -> String
    where
        F: FnMut(AgentEvent),
    {
        let tool = match self.tools.iter().find(|t| t.name() == call.name) {
            Some(t) => t,
            None => return format!("Error: Tool '{}' not found", call.name),
        };

        if self.get_mode() == AgentMode::Plan && tool.access() != ToolAccess::ReadOnly {
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
                on_event(AgentEvent::PermissionRequest(request.clone()));

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

        match tool.call(&call.arguments).await {
            Ok(output) => output,
            Err(err) => format!("Error executing {}: {}", call.name, err),
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
                    tool_calls: Some(vec![ToolCall {
                        id: "call".to_string(),
                        name: "write_test".to_string(),
                        arguments: "{}".to_string(),
                    }]),
                    tool_call_id: None,
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

    fn agent() -> Agent {
        Agent::new(Arc::new(TestProvider), Vec::new(), AgentMode::Build)
    }

    #[test]
    fn goal_is_injected_into_system_prompt() {
        let agent = agent();
        agent.set_goal("ship the harness");

        let prompt = agent.build_system_prompt();

        assert!(prompt.contains("ship the harness"));
        assert!(prompt.contains(GOAL_COMPLETE_MARKER));
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
        agent.set_goal("verify behavior");
        assert_eq!(agent.get_goal().unwrap().status, GoalStatus::Active);

        agent.complete_goal();
        assert_eq!(agent.get_goal().unwrap().status, GoalStatus::Completed);

        agent.clear_goal();
        assert_eq!(agent.get_goal(), None);
    }

    #[tokio::test]
    async fn goal_checklist_controls_completion_readiness() {
        let agent = agent();
        agent.set_goal("ship verified work");
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
        agent.set_goal("track work");
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
        agent.set_goal("track work");
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
        agent.set_goal("track");
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
        );
        let mut messages = vec![Message::new(Role::User, "run")];
        let mut events = Vec::new();

        let response = agent
            .run_streaming_with_events(&mut messages, |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(response.content, "done");
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
            AgentEvent::ToolCall { name, arguments }
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
        );
        let call = ToolCall {
            id: "call".to_string(),
            name: "write_test".to_string(),
            arguments: "{}".to_string(),
        };

        assert!(agent
            .execute_tool(&call, &mut |_| {})
            .await
            .contains("[Plan mode]"));
    }

    #[tokio::test]
    async fn write_tool_waits_for_permission_and_always_is_cached() {
        let agent = Arc::new(Agent::new(
            Arc::new(TestProvider),
            vec![Arc::new(WriteTestTool)],
            AgentMode::Build,
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
                .execute_tool(&task_call, &mut |event| {
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
            .execute_tool(&call, &mut |event| {
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
                .execute_tool(&call, &mut |event| {
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
        let agent = Agent::new(
            Arc::new(PermissionTestProvider(AtomicUsize::new(0))),
            vec![Arc::new(WriteTestTool)],
            AgentMode::Build,
        );
        let mut messages = vec![Message::new(Role::User, "write something")];

        let response = agent.run(&mut messages).await.unwrap();

        assert_eq!(response.content, "done");
        assert!(messages
            .iter()
            .any(|message| message.content.contains("Permission denied")));
    }
}
