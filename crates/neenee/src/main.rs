use neenee_core::commands::{discover_commands, expand_command, CustomCommand};
use neenee_core::skills::Skill;
use neenee_core::{
    async_trait,
    mcp::load_mcp_tools,
    parse_retryable_error,
    project::{init_neenee_config, CreateProjectTool, InitConfigTool},
    providers::{
        DeepSeekProvider, GLMProvider, GeminiProvider, KimiCodeProvider, KimiProvider,
        LlamaServerProvider, MockProvider, OpenAIProvider, QwenProvider, VolcengineProvider,
    },
    tools::{
        BashTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool, TaskTool,
        TodoWriteTool, UseSkillTool, WebFetchTool, WebSearchTool, WriteFileTool,
    },
    Agent, AgentEvent, AgentMode, AgentRequest, AgentResponse, Goal, GoalStatus, HarnessSnapshot,
    Message, Provider, ProviderStreamEvent, Role, SessionOverview, GOAL_COMPLETE_MARKER,
};
use neenee_tui::start_tui;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::sync::{Mutex, RwLock};
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

struct ProxyProvider {
    holder: Arc<RwLock<Arc<dyn Provider>>>,
}

#[async_trait]
impl Provider for ProxyProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn neenee_core::Tool>]) {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner());
        p.prepare_tools(tools);
    }

    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        p.chat(messages).await
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        p.stream_chat(messages).await
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<futures::stream::BoxStream<'static, Result<ProviderStreamEvent, String>>, String>
    {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        p.stream_chat_events(messages).await
    }
}

mod config;
mod session;
use config::Config;
use session::{
    compact_messages, discard_trailing_loop_prompts, estimate_chars, CompactionCheckpoint,
    LoopCheckpoint, SessionStore,
};

fn send_harness_state(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    agent: &Agent,
    loop_status: impl Into<String>,
) {
    let _ = tx.send(AgentResponse::HarnessState(HarnessSnapshot {
        mode: agent.get_mode(),
        goal: agent.get_goal(),
        loop_status: loop_status.into(),
    }));
}

fn persist_goal(agent: &Agent) {
    let mut config = Config::load();
    if let Some(goal) = agent.get_goal() {
        config.harness_goal = Some(goal.objective);
        config.harness_goal_completed = goal.status == GoalStatus::Completed;
        config.harness_goal_checklist = goal.checklist;
    } else {
        config.harness_goal = None;
        config.harness_goal_completed = false;
        config.harness_goal_checklist.clear();
    }
    let _ = config.save();
}

#[derive(Clone)]
struct TurnContext {
    agent: Arc<Agent>,
    history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    tx: mpsc::UnboundedSender<AgentResponse>,
    token: CancellationToken,
    session: Arc<SessionStore>,
    compaction_max_chars: usize,
    compaction_preserve_turns: usize,
    retry_max_attempts: usize,
    retry_base_ms: u64,
    retry_max_ms: u64,
}

struct TurnInput {
    prompt: String,
    hidden: bool,
    display_prompt: Option<String>,
}

#[derive(Clone)]
struct InteractiveTurnContext {
    agent: Arc<Agent>,
    history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    tx: mpsc::UnboundedSender<AgentResponse>,
    token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_counter: Arc<AtomicU64>,
    session: Arc<SessionStore>,
    compaction_max_chars: usize,
    compaction_preserve_turns: usize,
    retry_max_attempts: usize,
    retry_base_ms: u64,
    retry_max_ms: u64,
}

async fn start_interactive_turn(context: InteractiveTurnContext, input: TurnInput) {
    let token = CancellationToken::new();
    let generation = context.generation_counter.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(previous) = context.token_slot.write().await.replace(token.clone()) {
        context.agent.reject_pending_permissions();
        let _ = context.tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }
    let _ = context
        .tx
        .send(AgentResponse::Activity("starting request".to_string()));

    tokio::spawn(async move {
        send_harness_state(&context.tx, &context.agent, "running");
        let result = execute_turn(
            TurnContext {
                agent: context.agent.clone(),
                history: context.history,
                tx: context.tx.clone(),
                token: token.clone(),
                session: context.session,
                compaction_max_chars: context.compaction_max_chars,
                compaction_preserve_turns: context.compaction_preserve_turns,
                retry_max_attempts: context.retry_max_attempts,
                retry_base_ms: context.retry_base_ms,
                retry_max_ms: context.retry_max_ms,
            },
            input,
        )
        .await;
        let is_current = context.generation_counter.load(Ordering::SeqCst) == generation;
        match result {
            Ok(_) => {}
            Err(error) if error == "Interrupted" && is_current => {
                let _ = context
                    .tx
                    .send(AgentResponse::Text("... [Interrupted]".to_string()));
            }
            Err(error) if is_current => {
                let _ = context.tx.send(AgentResponse::Error(error));
            }
            Err(_) => {}
        }
        let mut slot = context.token_slot.write().await;
        if context.generation_counter.load(Ordering::SeqCst) == generation {
            slot.take();
            send_harness_state(&context.tx, &context.agent, "idle");
        }
    });
}

const BUILTIN_COMMANDS: &[&str] = &[
    "models",
    "mode",
    "mcp",
    "permissions",
    "session",
    "sessions",
    "resume",
    "compact",
    "goal",
    "loop",
    "init",
    "clear",
    "help",
    "exit",
];

fn split_custom_command(input: &str) -> (&str, &str) {
    let input = input.trim();
    let split_at = input.find(char::is_whitespace).unwrap_or(input.len());
    let (name, arguments) = input.split_at(split_at);
    (name.trim_start_matches('/'), arguments.trim())
}

async fn resume_session(
    session: &SessionStore,
    history: &tokio::sync::Mutex<Vec<Message>>,
    id: Option<&str>,
) -> Result<(String, Vec<Message>), String> {
    let id = session.resume(id).await?;
    *history.lock().await = session.messages().await;
    Ok((id, session.transcript().await))
}

async fn execute_turn(context: TurnContext, input: TurnInput) -> Result<bool, String> {
    let TurnContext {
        agent,
        history,
        tx,
        token,
        session,
        compaction_max_chars,
        compaction_preserve_turns,
        retry_max_attempts,
        retry_base_ms,
        retry_max_ms,
    } = context;
    let _ = tx.send(AgentResponse::Activity("saving request".to_string()));
    let admitted_session_id = session.id().await;
    let mut turn_history = {
        let mut history = history.lock().await;
        history.push(if input.hidden {
            Message::hidden(Role::User, input.prompt)
        } else {
            let message = Message::new(Role::User, input.prompt);
            match input.display_prompt {
                Some(display) => message.with_display_content(display),
                None => message,
            }
        });
        history.clone()
    };
    session.replace_messages(turn_history.clone()).await?;
    let _ = tx.send(AgentResponse::Activity("preparing context".to_string()));
    if estimate_chars(&turn_history) > compaction_max_chars {
        let _ = tx.send(AgentResponse::Activity("compacting context".to_string()));
        if let Some(checkpoint) = compact_turn_history(
            &mut turn_history,
            &session,
            compaction_max_chars,
            compaction_preserve_turns,
        )
        .await?
        {
            send_compaction(&tx, &checkpoint);
        }
        let _ = tx.send(AgentResponse::Activity("preparing context".to_string()));
    }

    let tool_activity = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let streamed_text = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut attempt: usize = 0;
    let retry_limit = retry_max_attempts.clamp(1, 10);
    let mut compacted_after_overflow = false;
    let result = loop {
        attempt += 1;
        let activity_for_run = tool_activity.clone();
        let streamed_for_run = streamed_text.clone();
        let result = tokio::select! {
            _ = token.cancelled() => return Err("Interrupted".to_string()),
            result = agent.run_streaming_with_events(&mut turn_history, |event| {
                if matches!(event, AgentEvent::ToolCall { .. }) {
                    activity_for_run.store(true, Ordering::SeqCst);
                }
                relay_agent_event(&tx, event, &streamed_for_run);
            }) => result,
        };

        let Err(error) = result else {
            break result;
        };
        if is_context_overflow(&error)
            && !compacted_after_overflow
            && !tool_activity.load(Ordering::SeqCst)
        {
            let _ = tx.send(AgentResponse::Activity("compacting context".to_string()));
            if compact_turn_history(
                &mut turn_history,
                &session,
                compaction_max_chars,
                compaction_preserve_turns.max(1),
            )
            .await?
            .is_some()
            {
                compacted_after_overflow = true;
                if streamed_text.swap(false, Ordering::SeqCst) {
                    let _ = tx.send(AgentResponse::StreamDiscard);
                }
                if let Some(checkpoint) = session.compaction().await {
                    send_compaction(&tx, &checkpoint);
                }
                attempt = attempt.saturating_sub(1);
                continue;
            }
        }

        let Some(retry) = parse_retryable_error(&error) else {
            break Err(error);
        };
        if tool_activity.load(Ordering::SeqCst) || attempt >= retry_limit {
            break Err(retry.message);
        }
        if streamed_text.swap(false, Ordering::SeqCst) {
            let _ = tx.send(AgentResponse::StreamDiscard);
        }
        let delay_ms = retry_delay_ms(attempt, retry.retry_after_ms, retry_base_ms, retry_max_ms);
        let _ = tx.send(AgentResponse::RetryScheduled {
            attempt: attempt + 1,
            max_attempts: retry_limit,
            delay_ms,
            message: retry.message,
        });
        tokio::select! {
            _ = token.cancelled() => return Err("Interrupted".to_string()),
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
        }
    };
    if session.id().await != admitted_session_id {
        return Err("Interrupted".to_string());
    }
    let _ = tx.send(AgentResponse::Activity("saving response".to_string()));
    *history.lock().await = turn_history.clone();
    session.replace_messages(turn_history).await?;
    let result = result?;

    let requested_completion = result.content.contains(GOAL_COMPLETE_MARKER);
    let completed = requested_completion && agent.goal_can_complete();
    let visible = result
        .content
        .replace(GOAL_COMPLETE_MARKER, "")
        .trim()
        .to_string();
    if !visible.is_empty() && !streamed_text.load(Ordering::SeqCst) {
        let _ = tx.send(AgentResponse::Text(visible));
    }
    if requested_completion && !completed {
        let _ = tx.send(AgentResponse::Text(
            "Goal completion was deferred because the checklist still has unfinished items."
                .to_string(),
        ));
    }
    if completed {
        agent.complete_goal();
        persist_goal(&agent);
    }
    Ok(completed)
}

fn retry_delay_ms(attempt: usize, retry_after_ms: Option<u64>, base_ms: u64, max_ms: u64) -> u64 {
    let exponent = attempt.saturating_sub(1).min(20) as u32;
    retry_after_ms
        .unwrap_or_else(|| base_ms.saturating_mul(2u64.saturating_pow(exponent)))
        .min(max_ms.max(1))
}

fn relay_agent_event(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    event: AgentEvent,
    streamed_text: &std::sync::atomic::AtomicBool,
) {
    let response = match event {
        AgentEvent::ModelRequestStarted { tool_round } => {
            let status = if tool_round == 0 {
                "waiting for model".to_string()
            } else {
                format!("waiting for model · round {}", tool_round + 1)
            };
            AgentResponse::Activity(status)
        }
        AgentEvent::AssistantDelta { delta, start } => {
            if start {
                let _ = tx.send(AgentResponse::StreamStart);
            }
            streamed_text.store(true, Ordering::SeqCst);
            AgentResponse::StreamDelta(delta)
        }
        AgentEvent::AssistantEnd(content) => {
            AgentResponse::StreamEnd(content.replace(GOAL_COMPLETE_MARKER, "").trim().to_string())
        }
        AgentEvent::AssistantDiscard => AgentResponse::StreamDiscard,
        AgentEvent::ReasoningDelta { delta, start } => {
            if start {
                let _ = tx.send(AgentResponse::StreamStart);
            }
            streamed_text.store(true, Ordering::SeqCst);
            AgentResponse::StreamReasoningDelta(delta)
        }
        AgentEvent::ReasoningEnd(content) => AgentResponse::StreamReasoningEnd(content),
        AgentEvent::ToolCall {
            id,
            name,
            arguments,
        } => AgentResponse::ToolCall {
            id,
            name,
            arguments,
        },
        AgentEvent::ToolResult {
            id,
            name,
            output,
            duration_ms,
        } => AgentResponse::ToolResult {
            id,
            name,
            output,
            duration_ms,
        },
        AgentEvent::GoalUpdated(goal) => {
            persist_goal_snapshot(&goal);
            AgentResponse::GoalUpdated(goal)
        }
        AgentEvent::PermissionRequest(request) => AgentResponse::PermissionRequest(request),
        AgentEvent::SubTask {
            parent_call_id,
            event,
        } => AgentResponse::SubTask {
            parent_call_id,
            event,
        },
    };
    let _ = tx.send(response);
}

fn persist_goal_snapshot(goal: &Goal) {
    let mut config = Config::load();
    config.harness_goal = Some(goal.objective.clone());
    config.harness_goal_completed = goal.status == GoalStatus::Completed;
    config.harness_goal_checklist = goal.checklist.clone();
    let _ = config.save();
}

async fn compact_turn_history(
    history: &mut Vec<Message>,
    session: &SessionStore,
    max_chars: usize,
    preserve_turns: usize,
) -> Result<Option<CompactionCheckpoint>, String> {
    let Some(result) = compact_messages(history, max_chars, preserve_turns) else {
        return Ok(None);
    };
    let checkpoint = result.checkpoint.clone();
    *history = result.active.clone();
    session.commit_compaction(result).await?;
    Ok(Some(checkpoint))
}

fn send_compaction(tx: &mpsc::UnboundedSender<AgentResponse>, checkpoint: &CompactionCheckpoint) {
    let _ = tx.send(AgentResponse::Compacted {
        archived_messages: checkpoint.archived_messages,
        before_chars: checkpoint.before_chars,
        after_chars: checkpoint.after_chars,
    });
}

fn is_context_overflow(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "context_window",
        "maximum context",
        "too many tokens",
        "token limit",
    ]
    .iter()
    .any(|pattern| error.contains(pattern))
}

fn short_session_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[derive(Clone)]
struct LoopRunContext {
    agent: Arc<Agent>,
    history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    tx: mpsc::UnboundedSender<AgentResponse>,
    token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_counter: Arc<AtomicU64>,
    session: Arc<SessionStore>,
    compaction_max_chars: usize,
    compaction_preserve_turns: usize,
    retry_max_attempts: usize,
    retry_base_ms: u64,
    retry_max_ms: u64,
}

async fn start_goal_loop(
    context: LoopRunContext,
    goal: String,
    start_iteration: usize,
    max_iterations: usize,
) {
    let token = CancellationToken::new();
    let generation = context.generation_counter.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(previous) = context.token_slot.write().await.replace(token.clone()) {
        context.agent.reject_pending_permissions();
        let _ = context.tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }

    send_harness_state(
        &context.tx,
        &context.agent,
        format!(
            "loop {}/{}",
            start_iteration.saturating_sub(1),
            max_iterations
        ),
    );

    tokio::spawn(async move {
        let mut terminal_status = "exhausted";
        for iteration in start_iteration..=max_iterations {
            let _ = context
                .session
                .set_checkpoint(Some(LoopCheckpoint {
                    goal: goal.clone(),
                    iteration,
                    max_iterations,
                    status: "running".to_string(),
                }))
                .await;
            send_harness_state(
                &context.tx,
                &context.agent,
                format!("loop {}/{}", iteration, max_iterations),
            );
            let prompt = format!(
                "Autonomous goal loop iteration {}/{}.\n\
                 Goal: {}\n\
                 Continue making concrete progress. Inspect the current state, use tools, \
                 implement and verify work. Do not stop at a plan. Emit {} only if the \
                 entire goal is achieved and verified.",
                iteration, max_iterations, goal, GOAL_COMPLETE_MARKER
            );
            match execute_turn(
                TurnContext {
                    agent: context.agent.clone(),
                    history: context.history.clone(),
                    tx: context.tx.clone(),
                    token: token.clone(),
                    session: context.session.clone(),
                    compaction_max_chars: context.compaction_max_chars,
                    compaction_preserve_turns: context.compaction_preserve_turns,
                    retry_max_attempts: context.retry_max_attempts,
                    retry_base_ms: context.retry_base_ms,
                    retry_max_ms: context.retry_max_ms,
                },
                TurnInput {
                    prompt,
                    hidden: true,
                    display_prompt: None,
                },
            )
            .await
            {
                Ok(true) => {
                    terminal_status = "completed";
                    let _ = context
                        .session
                        .set_checkpoint(Some(LoopCheckpoint {
                            goal: goal.clone(),
                            iteration,
                            max_iterations,
                            status: terminal_status.to_string(),
                        }))
                        .await;
                    let _ = context.tx.send(AgentResponse::Text(format!(
                        "Goal completed in loop iteration {}.",
                        iteration
                    )));
                    break;
                }
                Ok(false) if iteration == max_iterations => {
                    let _ = context
                        .session
                        .set_checkpoint(Some(LoopCheckpoint {
                            goal: goal.clone(),
                            iteration,
                            max_iterations,
                            status: terminal_status.to_string(),
                        }))
                        .await;
                    let _ = context.tx.send(AgentResponse::Text(format!(
                        "Loop exhausted its {} iteration budget. Continue with /loop <N> or set a new goal.",
                        max_iterations
                    )));
                }
                Ok(false) => {}
                Err(error) if error == "Interrupted" => {
                    terminal_status = "interrupted";
                    let _ = context
                        .session
                        .set_checkpoint(Some(LoopCheckpoint {
                            goal: goal.clone(),
                            iteration,
                            max_iterations,
                            status: terminal_status.to_string(),
                        }))
                        .await;
                    let _ = context
                        .tx
                        .send(AgentResponse::Text("Loop interrupted.".to_string()));
                    break;
                }
                Err(error) => {
                    terminal_status = "error";
                    let _ = context
                        .session
                        .set_checkpoint(Some(LoopCheckpoint {
                            goal: goal.clone(),
                            iteration,
                            max_iterations,
                            status: terminal_status.to_string(),
                        }))
                        .await;
                    let _ = context.tx.send(AgentResponse::Error(error));
                    break;
                }
            }
        }

        let mut slot = context.token_slot.write().await;
        if context.generation_counter.load(Ordering::SeqCst) == generation {
            slot.take();
            send_harness_state(&context.tx, &context.agent, "idle");
        }
    });
}

/// Whether each provider has a usable API key (env var or config).
/// Keyless providers (local llama, mock) always report `true`.
fn provider_key_status(config: &Config) -> Vec<(String, bool)> {
    fn has(env: &str, cfg: &Option<String>) -> bool {
        std::env::var(env)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some()
            || cfg.as_deref().is_some_and(|v| !v.trim().is_empty())
    }
    vec![
        (
            "openai".to_string(),
            has("OPENAI_API_KEY", &config.openai_api_key),
        ),
        (
            "gemini".to_string(),
            has("GEMINI_API_KEY", &config.gemini_api_key),
        ),
        (
            "kimi-code".to_string(),
            has("KIMI_CODE_API_KEY", &config.kimi_code_api_key),
        ),
        (
            "kimi".to_string(),
            has("KIMI_API_KEY", &config.kimi_api_key),
        ),
        (
            "deepseek".to_string(),
            has("DEEPSEEK_API_KEY", &config.deepseek_api_key),
        ),
        (
            "qwen".to_string(),
            has("DASHSCOPE_API_KEY", &config.qwen_api_key),
        ),
        ("glm".to_string(), has("GLM_API_KEY", &config.glm_api_key)),
        (
            "volcengine".to_string(),
            has("VOLCENGINE_API_KEY", &config.volcengine_api_key),
        ),
        ("llama".to_string(), true),
        (
            "custom".to_string(),
            has("CUSTOM_API_KEY", &config.custom_api_key),
        ),
        ("mock".to_string(), true),
    ]
}

#[derive(Debug)]
enum StartupMode {
    Fresh,
    Resume(Option<String>),
    Picker,
}

fn parse_args(args: Vec<String>) -> StartupMode {
    match args.as_slice() {
        [] => StartupMode::Fresh,
        [cmd] if cmd == "resume" => StartupMode::Picker,
        [cmd, id] if cmd == "resume" => StartupMode::Resume(Some(id.clone())),
        [cmd, ..] => {
            eprintln!(
                "Unknown command '{}'. Usage:\n  neenee              start a fresh session\n  neenee resume [id]  resume a session (picker when no id)",
                cmd
            );
            std::process::exit(2);
        }
    }
}

async fn build_sessions_overview(session: &SessionStore) -> Vec<SessionOverview> {
    match session.list().await {
        Ok(items) => items
            .into_iter()
            .map(|item| SessionOverview {
                id: item.id,
                overview: item.overview,
                created_at: item.created_at,
                updated_at: item.updated_at,
                message_count: item.message_count,
                active: item.active,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<AgentRequest>();
    let (resp_tx, resp_rx) = mpsc::unbounded_channel::<AgentResponse>();
    let custom_commands = discover_commands()
        .into_iter()
        .filter(|command| !BUILTIN_COMMANDS.contains(&command.name.as_str()))
        .map(|command| (command.name.clone(), command))
        .collect::<HashMap<String, CustomCommand>>();
    let custom_command_suggestions = {
        let mut suggestions = custom_commands
            .values()
            .map(|command| {
                (
                    format!("/{}", command.name),
                    command
                        .description
                        .clone()
                        .unwrap_or_else(|| "Run project command".to_string()),
                )
            })
            .collect::<Vec<_>>();
        suggestions.sort_by(|left, right| left.0.cmp(&right.0));
        suggestions
    };

    let mut config = Config::load();

    // Initialize Agent logic
    let initial_provider: Arc<dyn Provider> = match config.default_provider.as_str() {
        "llama" => Arc::new(LlamaServerProvider::new(
            std::env::var("LLAMA_BASE_URL")
                .ok()
                .or(config.llama_base_url.clone())
                .unwrap_or_else(|| "http://localhost:8080".to_string()),
            std::env::var("LLAMA_MODEL")
                .ok()
                .or(config.llama_model.clone())
                .unwrap_or_else(|| "local-model".to_string()),
        )),
        "gemini" => Arc::new(GeminiProvider::new(
            std::env::var("GEMINI_API_KEY")
                .ok()
                .or(config.gemini_api_key.clone())
                .unwrap_or_default(),
            std::env::var("GEMINI_MODEL")
                .ok()
                .or(config.gemini_model.clone())
                .unwrap_or_else(|| "gemini-1.5-flash".to_string()),
        )),
        "openai" => Arc::new(OpenAIProvider::new(
            std::env::var("OPENAI_API_KEY")
                .ok()
                .or(config.openai_api_key.clone())
                .unwrap_or_default(),
            std::env::var("OPENAI_MODEL")
                .ok()
                .or(config.openai_model.clone())
                .unwrap_or_else(|| "gpt-4o".to_string()),
        )),
        "kimi-code" => Arc::new(KimiCodeProvider::new(
            std::env::var("KIMI_CODE_API_KEY")
                .ok()
                .or(config.kimi_code_api_key.clone())
                .unwrap_or_default(),
            std::env::var("KIMI_CODE_USER_AGENT")
                .ok()
                .or(config.kimi_code_user_agent.clone())
                .unwrap_or_else(|| KimiCodeProvider::OPENCODE_USER_AGENT.to_string()),
        )),
        "kimi" => Arc::new(KimiProvider::new(
            std::env::var("KIMI_API_KEY")
                .ok()
                .or(config.kimi_api_key.clone())
                .unwrap_or_default(),
            std::env::var("KIMI_MODEL")
                .ok()
                .or(config.kimi_model.clone())
                .unwrap_or_else(|| "moonshot-v1-8k".to_string()),
        )),
        "deepseek" => Arc::new(DeepSeekProvider::new(
            std::env::var("DEEPSEEK_API_KEY")
                .ok()
                .or(config.deepseek_api_key.clone())
                .unwrap_or_default(),
            std::env::var("DEEPSEEK_MODEL")
                .ok()
                .or(config.deepseek_model.clone())
                .unwrap_or_else(|| "deepseek-chat".to_string()),
        )),
        "qwen" => Arc::new(QwenProvider::new(
            std::env::var("DASHSCOPE_API_KEY")
                .ok()
                .or(config.qwen_api_key.clone())
                .unwrap_or_default(),
            std::env::var("QWEN_MODEL")
                .ok()
                .or(config.qwen_model.clone())
                .unwrap_or_else(|| "qwen-plus".to_string()),
        )),
        "glm" => Arc::new(GLMProvider::new(
            std::env::var("GLM_API_KEY")
                .ok()
                .or(config.glm_api_key.clone())
                .unwrap_or_default(),
            std::env::var("GLM_MODEL")
                .ok()
                .or(config.glm_model.clone())
                .unwrap_or_else(|| "glm-4-plus".to_string()),
        )),
        "volcengine" => Arc::new(VolcengineProvider::new(
            std::env::var("VOLCENGINE_API_KEY")
                .ok()
                .or(config.volcengine_api_key.clone())
                .unwrap_or_default(),
            std::env::var("VOLCENGINE_MODEL")
                .ok()
                .or(config.volcengine_model.clone())
                .unwrap_or_else(|| "deepseek-v3-250324".to_string()),
        )),
        "custom" => Arc::new(OpenAIProvider::with_base_url(
            std::env::var("CUSTOM_API_KEY")
                .ok()
                .or(config.custom_api_key.clone())
                .unwrap_or_default(),
            std::env::var("CUSTOM_MODEL")
                .ok()
                .or(config.custom_model.clone())
                .unwrap_or_else(|| "custom-model".to_string()),
            &std::env::var("CUSTOM_BASE_URL")
                .ok()
                .or(config.custom_base_url.clone())
                .unwrap_or_else(|| "http://localhost:8080/v1/chat/completions".to_string()),
        )),
        _ => Arc::new(MockProvider),
    };

    let provider_holder = Arc::new(RwLock::new(initial_provider));
    let provider_for_task = provider_holder.clone();

    let agent_provider = Arc::new(ProxyProvider {
        holder: provider_holder,
    });

    // Shared skills registry for the use_skill tool
    let skills_registry: Arc<Mutex<Vec<Skill>>> = Arc::new(Mutex::new(Vec::new()));
    let _skills_for_agent = skills_registry.clone();

    let mcp = load_mcp_tools(&config.mcp).await;
    let mcp_statuses = mcp.statuses;

    let mut tools: Vec<Arc<dyn neenee_core::Tool>> = vec![
        Arc::new(BashTool),
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(EditFileTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
        Arc::new(ListDirTool),
        Arc::new(WebFetchTool),
        Arc::new(WebSearchTool),
        Arc::new(TodoWriteTool::new()),
        Arc::new(CreateProjectTool),
        Arc::new(InitConfigTool),
        Arc::new(UseSkillTool {
            skills: skills_registry.clone(),
        }),
    ];
    tools.extend(mcp.tools);
    // TaskTool gets a snapshot of the toolset (excluding itself) so spawned
    // sub-agents cannot recurse and inherit the live provider.
    let task_tool = Arc::new(TaskTool::new(agent_provider.clone(), tools.clone()));
    tools.push(task_tool);
    let agent = Arc::new(Agent::new(agent_provider, tools, AgentMode::Build));
    if let Some(objective) = config.harness_goal.clone() {
        agent.restore_goal(Goal {
            objective,
            status: if config.harness_goal_completed {
                GoalStatus::Completed
            } else {
                GoalStatus::Active
            },
            checklist: config.harness_goal_checklist.clone(),
        });
    }

    // Sync skills from agent into the shared registry so use_skill can find them
    {
        let agent_skills = agent.skills.clone();
        let mut reg = skills_registry.lock().unwrap();
        *reg = agent_skills;
    }

    // CLI: `neenee` -> fresh session; `neenee resume [id]` -> resume a session.
    let startup: StartupMode = parse_args(std::env::args().skip(1).collect());

    // Session loading honors the startup mode. The previous active session is
    // archived and remains available through /resume or /session resume.
    let session = Arc::new(SessionStore::load());
    let open_picker_on_start = match &startup {
        StartupMode::Fresh => {
            session.reset().await.map_err(std::io::Error::other)?;
            false
        }
        StartupMode::Picker => {
            session.reset().await.map_err(std::io::Error::other)?;
            true
        }
        StartupMode::Resume(id) => {
            if let Err(error) = session.resume(id.as_deref()).await {
                eprintln!("resume failed: {error}; starting a fresh session.");
                session.reset().await.map_err(std::io::Error::other)?;
            }
            false
        }
    };
    let active_messages = session.messages().await;
    let restored_messages = session.transcript().await;
    let history = Arc::new(tokio::sync::Mutex::new(active_messages));

    // Load history
    let input_history = Config::load_history();

    let current_task_token = Arc::new(AsyncRwLock::new(None::<CancellationToken>));
    let task_generation = Arc::new(AtomicU64::new(0));
    let ctt_clone = current_task_token.clone();
    let generation_clone = task_generation.clone();
    let commands_for_task = Arc::new(custom_commands);

    // Initial values for TUI
    let initial_p_name = config.default_provider.clone();
    let initial_m_name = match initial_p_name.as_str() {
        "openai" => config
            .openai_model
            .clone()
            .unwrap_or_else(|| "gpt-4o".to_string()),
        "gemini" => config
            .gemini_model
            .clone()
            .unwrap_or_else(|| "gemini-1.5-flash".to_string()),
        "llama" => config
            .llama_model
            .clone()
            .unwrap_or_else(|| "local-model".to_string()),
        "kimi-code" => KimiCodeProvider::MODEL.to_string(),
        "kimi" => config
            .kimi_model
            .clone()
            .unwrap_or_else(|| "moonshot-v1-8k".to_string()),
        "deepseek" => config
            .deepseek_model
            .clone()
            .unwrap_or_else(|| "deepseek-chat".to_string()),
        "qwen" => config
            .qwen_model
            .clone()
            .unwrap_or_else(|| "qwen-plus".to_string()),
        "glm" => config
            .glm_model
            .clone()
            .unwrap_or_else(|| "glm-4-plus".to_string()),
        "volcengine" => config
            .volcengine_model
            .clone()
            .unwrap_or_else(|| "deepseek-v3-250324".to_string()),
        "custom" => config
            .custom_model
            .clone()
            .unwrap_or_else(|| "custom-model".to_string()),
        _ => "mock-model".to_string(),
    };

    // Spawn Agent Background Task
    tokio::spawn(async move {
        send_harness_state(&resp_tx, &agent, "idle");
        let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
        if open_picker_on_start {
            let _ = resp_tx.send(AgentResponse::SessionsOverview(
                build_sessions_overview(&session).await,
            ));
        }
        while let Some(req) = req_rx.recv().await {
            match req {
                AgentRequest::Interrupt => {
                    // Cancellation is driven by the token below; we deliberately
                    // do NOT bump the generation counter here. Bumping would make
                    // the in-flight turn's `is_current` check false, so its own
                    // cleanup (the "... [Interrupted]" message and the transition
                    // back to "idle") would be skipped — leaving the UI stuck in
                    // the "running" state with no interruption feedback. A later
                    // turn bumps the generation itself and supersedes this one.
                    agent.reject_pending_permissions();
                    let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                    let mut token = ctt_clone.write().await;
                    if let Some(t) = token.take() {
                        t.cancel();
                    }
                }
                AgentRequest::PermissionReply {
                    request_id,
                    decision,
                } => {
                    if !agent.reply_permission(&request_id, decision) {
                        let _ = resp_tx.send(AgentResponse::Error(
                            "Permission request is no longer pending.".to_string(),
                        ));
                    }
                }
                AgentRequest::SwitchProvider {
                    provider_type,
                    model,
                    api_key,
                    base_url,
                } => {
                    // A key entered in the TUI is persisted and wins over
                    // config; environment variables still take precedence.
                    if let Some(key) = api_key.clone() {
                        match provider_type.as_str() {
                            "openai" => config.openai_api_key = Some(key),
                            "gemini" => config.gemini_api_key = Some(key),
                            "kimi-code" => config.kimi_code_api_key = Some(key),
                            "kimi" => config.kimi_api_key = Some(key),
                            "deepseek" => config.deepseek_api_key = Some(key),
                            "qwen" => config.qwen_api_key = Some(key),
                            "glm" => config.glm_api_key = Some(key),
                            "volcengine" => config.volcengine_api_key = Some(key),
                            "custom" => config.custom_api_key = Some(key),
                            _ => {}
                        }
                    }
                    if let Some(url) = base_url {
                        match provider_type.as_str() {
                            "llama" => config.llama_base_url = Some(url),
                            "custom" => config.custom_base_url = Some(url),
                            _ => {}
                        }
                    }
                    let new_p: Arc<dyn Provider> = match provider_type.as_str() {
                        "openai" => Arc::new(OpenAIProvider::new(
                            std::env::var("OPENAI_API_KEY")
                                .ok()
                                .or(config.openai_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "gemini" => Arc::new(GeminiProvider::new(
                            std::env::var("GEMINI_API_KEY")
                                .ok()
                                .or(config.gemini_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "kimi-code" => Arc::new(KimiCodeProvider::new(
                            std::env::var("KIMI_CODE_API_KEY")
                                .ok()
                                .or(config.kimi_code_api_key.clone())
                                .unwrap_or_default(),
                            std::env::var("KIMI_CODE_USER_AGENT")
                                .ok()
                                .or(config.kimi_code_user_agent.clone())
                                .unwrap_or_else(|| {
                                    KimiCodeProvider::OPENCODE_USER_AGENT.to_string()
                                }),
                        )),
                        "llama" => Arc::new(LlamaServerProvider::new(
                            std::env::var("LLAMA_BASE_URL")
                                .ok()
                                .or(config.llama_base_url.clone())
                                .unwrap_or_else(|| "http://localhost:8080".to_string()),
                            model.clone(),
                        )),
                        "kimi" => Arc::new(KimiProvider::new(
                            std::env::var("KIMI_API_KEY")
                                .ok()
                                .or(config.kimi_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "deepseek" => Arc::new(DeepSeekProvider::new(
                            std::env::var("DEEPSEEK_API_KEY")
                                .ok()
                                .or(config.deepseek_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "qwen" => Arc::new(QwenProvider::new(
                            std::env::var("DASHSCOPE_API_KEY")
                                .ok()
                                .or(config.qwen_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "glm" => Arc::new(GLMProvider::new(
                            std::env::var("GLM_API_KEY")
                                .ok()
                                .or(config.glm_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "volcengine" => Arc::new(VolcengineProvider::new(
                            std::env::var("VOLCENGINE_API_KEY")
                                .ok()
                                .or(config.volcengine_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                        )),
                        "custom" => Arc::new(OpenAIProvider::with_base_url(
                            std::env::var("CUSTOM_API_KEY")
                                .ok()
                                .or(config.custom_api_key.clone())
                                .unwrap_or_default(),
                            model.clone(),
                            &std::env::var("CUSTOM_BASE_URL")
                                .ok()
                                .or(config.custom_base_url.clone())
                                .unwrap_or_default(),
                        )),
                        _ => Arc::new(MockProvider),
                    };
                    *provider_for_task
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) = new_p;

                    // Update and save config
                    config.default_provider = provider_type.clone();
                    match provider_type.as_str() {
                        "openai" => config.openai_model = Some(model.clone()),
                        "gemini" => config.gemini_model = Some(model.clone()),
                        "kimi-code" => {}
                        "llama" => config.llama_model = Some(model.clone()),
                        "kimi" => config.kimi_model = Some(model.clone()),
                        "deepseek" => config.deepseek_model = Some(model.clone()),
                        "qwen" => config.qwen_model = Some(model.clone()),
                        "glm" => config.glm_model = Some(model.clone()),
                        "volcengine" => config.volcengine_model = Some(model.clone()),
                        "custom" => config.custom_model = Some(model.clone()),
                        _ => {}
                    }
                    let _ = config.save();

                    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
                    let _ = resp_tx.send(AgentResponse::ProviderSwitched {
                        provider: provider_type,
                        model,
                    });
                }
                AgentRequest::SlashCommand(cmd) => {
                    let parts: Vec<&str> = cmd.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }
                    match parts[0] {
                        "/models" => {
                            // Handled in TUI
                        }
                        "/mode" => {
                            if parts.len() > 1 {
                                let new_mode = match parts[1].to_lowercase().as_str() {
                                    "build" => AgentMode::Build,
                                    "plan" => AgentMode::Plan,
                                    _ => {
                                        let _ = resp_tx.send(AgentResponse::Error(format!(
                                            "Unknown mode '{}'. Use 'build' or 'plan'.",
                                            parts[1]
                                        )));
                                        continue;
                                    }
                                };
                                agent.set_mode(new_mode);
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Mode changed to: {:?}",
                                    new_mode
                                )));
                                send_harness_state(&resp_tx, &agent, "idle");
                            } else {
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Current mode: {:?}",
                                    agent.get_mode()
                                )));
                            }
                        }
                        "/mcp" => {
                            let message = if mcp_statuses.is_empty() {
                                "No MCP servers configured.".to_string()
                            } else {
                                format!(
                                    "MCP servers:\n{}",
                                    mcp_statuses
                                        .iter()
                                        .map(|(name, status)| format!("- {}: {}", name, status))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                )
                            };
                            let _ = resp_tx.send(AgentResponse::Text(message));
                        }
                        "/permissions" => {
                            if parts.get(1) == Some(&"clear") {
                                agent.clear_allowed_tools();
                                let _ = resp_tx.send(AgentResponse::Text(
                                    "Always-allowed tool rules cleared.".to_string(),
                                ));
                            } else {
                                let allowed = agent.allowed_tools();
                                let message = if allowed.is_empty() {
                                    "No tools are always allowed for this process.".to_string()
                                } else {
                                    format!("Always-allowed tools:\n- {}", allowed.join("\n- "))
                                };
                                let _ = resp_tx.send(AgentResponse::Text(message));
                            }
                        }
                        "/resume" => {
                            generation_clone.fetch_add(1, Ordering::SeqCst);
                            agent.reject_pending_permissions();
                            let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                            if let Some(token) = ctt_clone.write().await.take() {
                                token.cancel();
                            }
                            match resume_session(&session, &history, parts.get(1).copied()).await {
                                Ok((id, transcript)) => {
                                    let _ = resp_tx
                                        .send(AgentResponse::ConversationReplaced(transcript));
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Resumed session {}.",
                                        short_session_id(&id)
                                    )));
                                    send_harness_state(&resp_tx, &agent, "idle");
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(error));
                                }
                            }
                        }
                        "/session" => match parts.get(1).copied().unwrap_or("status") {
                            "status" => {
                                let id = session.id().await;
                                let parent_id = session
                                    .parent_id()
                                    .await
                                    .unwrap_or_else(|| "none".to_string());
                                let message_count = history.lock().await.len();
                                let archived_count = session.archived_count().await;
                                let checkpoint = session.checkpoint().await;
                                let compaction = session.compaction().await;
                                let checkpoint_text = checkpoint
                                    .map(|item| {
                                        format!(
                                            "{} {}/{} ({})",
                                            item.goal,
                                            item.iteration,
                                            item.max_iterations,
                                            item.status
                                        )
                                    })
                                    .unwrap_or_else(|| "none".to_string());
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Session: {}\nForked from: {}\nActive messages: {}\nArchived messages: {}\nLoop checkpoint: {}\nLast compaction: {}",
                                    id,
                                    parent_id,
                                    message_count,
                                    archived_count,
                                    checkpoint_text,
                                    compaction
                                        .map(|item| format!(
                                            "{} -> {} chars",
                                            item.before_chars, item.after_chars
                                        ))
                                        .unwrap_or_else(|| "none".to_string())
                                )));
                            }
                            "list" => match session.list().await {
                                Ok(sessions) => {
                                    let lines = sessions
                                        .into_iter()
                                        .map(|item| {
                                            format!(
                                                "- {}{}  messages={}  parent={}",
                                                short_session_id(&item.id),
                                                if item.active { " [active]" } else { "" },
                                                item.message_count,
                                                item.parent_id
                                                    .map(|id| short_session_id(&id).to_string())
                                                    .unwrap_or_else(|| "none".to_string())
                                            )
                                        })
                                        .collect::<Vec<_>>();
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Sessions:\n{}",
                                        lines.join("\n")
                                    )));
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(error));
                                }
                            },
                            "fork" => {
                                generation_clone.fetch_add(1, Ordering::SeqCst);
                                agent.reject_pending_permissions();
                                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                                if let Some(token) = ctt_clone.write().await.take() {
                                    token.cancel();
                                }
                                match session.fork().await {
                                    Ok((id, parent_id)) => {
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Forked session {} from {}.",
                                            id, parent_id
                                        )));
                                        send_harness_state(&resp_tx, &agent, "idle");
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            }
                            "open" => {
                                let Some(id) = parts.get(2) else {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Usage: /session open <session-id>".to_string(),
                                    ));
                                    continue;
                                };
                                generation_clone.fetch_add(1, Ordering::SeqCst);
                                agent.reject_pending_permissions();
                                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                                if let Some(token) = ctt_clone.write().await.take() {
                                    token.cancel();
                                }
                                match session.open(id).await {
                                    Ok(()) => {
                                        *history.lock().await = session.messages().await;
                                        let transcript = session.transcript().await;
                                        let _ = resp_tx
                                            .send(AgentResponse::ConversationReplaced(transcript));
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Opened session {}.",
                                            id
                                        )));
                                        send_harness_state(&resp_tx, &agent, "idle");
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            }
                            "resume" => {
                                generation_clone.fetch_add(1, Ordering::SeqCst);
                                agent.reject_pending_permissions();
                                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                                if let Some(token) = ctt_clone.write().await.take() {
                                    token.cancel();
                                }
                                match resume_session(&session, &history, parts.get(2).copied())
                                    .await
                                {
                                    Ok((id, transcript)) => {
                                        let _ = resp_tx
                                            .send(AgentResponse::ConversationReplaced(transcript));
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Resumed session {}.",
                                            short_session_id(&id)
                                        )));
                                        send_harness_state(&resp_tx, &agent, "idle");
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            }
                            "new" => {
                                generation_clone.fetch_add(1, Ordering::SeqCst);
                                agent.reject_pending_permissions();
                                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                                if let Some(token) = ctt_clone.write().await.take() {
                                    token.cancel();
                                }
                                history.lock().await.clear();
                                match session.reset().await {
                                    Ok(id) => {
                                        let _ = resp_tx.send(AgentResponse::ConversationCleared);
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Started new session: {}",
                                            id
                                        )));
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            }
                            other => {
                                let _ = resp_tx.send(AgentResponse::Error(format!(
                                    "Unknown session command '{}'. Use status, list, resume, fork, open, or new.",
                                    other
                                )));
                            }
                        },
                        "/sessions" => {
                            let _ = resp_tx.send(AgentResponse::SessionsOverview(
                                build_sessions_overview(&session).await,
                            ));
                        }
                        "/compact" => {
                            let mut current = history.lock().await.clone();
                            match compact_turn_history(
                                &mut current,
                                &session,
                                config.compaction_max_chars,
                                config.compaction_preserve_turns,
                            )
                            .await
                            {
                                Ok(Some(checkpoint)) => {
                                    *history.lock().await = current;
                                    send_compaction(&resp_tx, &checkpoint);
                                }
                                Ok(None) => {
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "Not enough complete turns to compact.".to_string(),
                                    ));
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(error));
                                }
                            }
                        }
                        "/goal" => {
                            let argument = cmd.strip_prefix("/goal").unwrap_or("").trim();
                            match argument {
                                "" | "status" => {
                                    let message = match agent.get_goal() {
                                        Some(goal) => format_goal_status(&goal),
                                        None => "No active goal. Set one with /goal <objective>."
                                            .to_string(),
                                    };
                                    let _ = resp_tx.send(AgentResponse::Text(message));
                                }
                                "clear" => {
                                    agent.clear_goal();
                                    config.harness_goal = None;
                                    config.harness_goal_completed = false;
                                    config.harness_goal_checklist.clear();
                                    let _ = config.save();
                                    let _ = resp_tx
                                        .send(AgentResponse::Text("Goal cleared.".to_string()));
                                }
                                "done" => {
                                    if agent.complete_goal().is_some() {
                                        config.harness_goal_completed = true;
                                        let _ = config.save();
                                        let _ = resp_tx.send(AgentResponse::Text(
                                            "Goal marked completed.".to_string(),
                                        ));
                                    } else {
                                        let _ = resp_tx.send(AgentResponse::Error(
                                            "No goal to complete.".to_string(),
                                        ));
                                    }
                                }
                                objective => {
                                    let goal = agent.set_goal(objective);
                                    config.harness_goal = Some(goal.objective.clone());
                                    config.harness_goal_completed = false;
                                    config.harness_goal_checklist.clear();
                                    let _ = config.save();
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Goal set: {}",
                                        goal.objective
                                    )));
                                }
                            }
                            send_harness_state(&resp_tx, &agent, "idle");
                        }
                        "/loop" => {
                            let argument = parts.get(1).copied().unwrap_or("8");
                            if argument == "stop" {
                                let mut current = ctt_clone.write().await;
                                if let Some(token) = current.take() {
                                    token.cancel();
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "Loop stop requested.".to_string(),
                                    ));
                                } else {
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "No loop is running.".to_string(),
                                    ));
                                }
                                send_harness_state(&resp_tx, &agent, "idle");
                                continue;
                            }
                            if argument == "status" {
                                let running = ctt_clone.read().await.is_some();
                                let status = if running { "running" } else { "idle" };
                                let checkpoint = session.checkpoint().await;
                                let detail = checkpoint
                                    .map(|checkpoint| {
                                        format!(
                                            "{} {}/{} for {}",
                                            checkpoint.status,
                                            checkpoint.iteration,
                                            checkpoint.max_iterations,
                                            checkpoint.goal
                                        )
                                    })
                                    .unwrap_or_else(|| "no checkpoint".to_string());
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Loop status: {}\nCheckpoint: {}",
                                    status, detail
                                )));
                                send_harness_state(&resp_tx, &agent, status);
                                continue;
                            }
                            if argument == "resume" {
                                if ctt_clone.read().await.is_some() {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "A chat or loop task is already running.".to_string(),
                                    ));
                                    continue;
                                }
                                let checkpoint = match session.checkpoint().await {
                                    Some(checkpoint) => checkpoint,
                                    None => {
                                        let _ = resp_tx.send(AgentResponse::Error(
                                            "No loop checkpoint is available to resume."
                                                .to_string(),
                                        ));
                                        continue;
                                    }
                                };
                                let start_iteration = match checkpoint.resume_iteration() {
                                    Ok(iteration) => iteration,
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
                                    }
                                };

                                let mut current = history.lock().await.clone();
                                let discarded = discard_trailing_loop_prompts(&mut current);
                                if discarded > 0 {
                                    *history.lock().await = current.clone();
                                    if let Err(error) = session.replace_messages(current).await {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
                                    }
                                }

                                let goal = agent.set_goal(checkpoint.goal.clone());
                                config.harness_goal = Some(goal.objective.clone());
                                config.harness_goal_completed = false;
                                let _ = config.save();
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Resuming goal loop at iteration {}/{}{}.",
                                    start_iteration,
                                    checkpoint.max_iterations,
                                    if discarded > 0 {
                                        " after removing an incomplete control prompt"
                                    } else {
                                        ""
                                    }
                                )));
                                start_goal_loop(
                                    LoopRunContext {
                                        agent: agent.clone(),
                                        history: history.clone(),
                                        tx: resp_tx.clone(),
                                        token_slot: ctt_clone.clone(),
                                        generation_counter: generation_clone.clone(),
                                        session: session.clone(),
                                        compaction_max_chars: config.compaction_max_chars,
                                        compaction_preserve_turns: config.compaction_preserve_turns,
                                        retry_max_attempts: config.provider_retry_max_attempts,
                                        retry_base_ms: config.provider_retry_base_ms,
                                        retry_max_ms: config.provider_retry_max_ms,
                                    },
                                    checkpoint.goal,
                                    start_iteration,
                                    checkpoint.max_iterations,
                                )
                                .await;
                                continue;
                            }

                            let max_iterations = match argument.parse::<usize>() {
                                Ok(value) if (1..=50).contains(&value) => value,
                                _ => {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Usage: /loop <1-50> | /loop resume | /loop stop | /loop status".to_string(),
                                    ));
                                    continue;
                                }
                            };
                            let goal = match agent.get_goal() {
                                Some(goal) if goal.status == GoalStatus::Active => goal,
                                _ => {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Set an active goal with /goal <objective> before starting /loop.".to_string()
                                    ));
                                    continue;
                                }
                            };
                            start_goal_loop(
                                LoopRunContext {
                                    agent: agent.clone(),
                                    history: history.clone(),
                                    tx: resp_tx.clone(),
                                    token_slot: ctt_clone.clone(),
                                    generation_counter: generation_clone.clone(),
                                    session: session.clone(),
                                    compaction_max_chars: config.compaction_max_chars,
                                    compaction_preserve_turns: config.compaction_preserve_turns,
                                    retry_max_attempts: config.provider_retry_max_attempts,
                                    retry_base_ms: config.provider_retry_base_ms,
                                    retry_max_ms: config.provider_retry_max_ms,
                                },
                                goal.objective,
                                1,
                                max_iterations,
                            )
                            .await;
                        }
                        "/init" => {
                            let target = parts.get(1).copied().unwrap_or(".");
                            match init_neenee_config(std::path::Path::new(target)) {
                                Ok(created) if created.is_empty() => {
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "neenee is already configured in '{}'. Nothing to do.",
                                        target
                                    )));
                                }
                                Ok(created) => {
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Initialized neenee configuration in '{}'.\nCreated:\n{}",
                                        target,
                                        created
                                            .iter()
                                            .map(|path| format!("- {}", path))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    )));
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(error));
                                }
                            }
                        }
                        "/clear" => {
                            history.lock().await.clear();
                            let _ = session.replace_messages(Vec::new()).await;
                            let _ = resp_tx.send(AgentResponse::ConversationCleared);
                            let _ = resp_tx.send(AgentResponse::Text(
                                "Conversation history cleared.".to_string(),
                            ));
                        }
                        "/help" => {
                            let custom_help = if commands_for_task.is_empty() {
                                String::new()
                            } else {
                                let mut commands = commands_for_task.values().collect::<Vec<_>>();
                                commands.sort_by(|left, right| left.name.cmp(&right.name));
                                format!(
                                    "\n\nProject commands:\n{}",
                                    commands
                                        .into_iter()
                                        .map(|command| format!(
                                            "/{} — {}",
                                            command.name,
                                            command
                                                .description
                                                .as_deref()
                                                .unwrap_or("Run project command")
                                        ))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                )
                            };
                            let _ = resp_tx.send(AgentResponse::Text(
                                format!("Slash commands:\n\
                                /models   — Select LLM provider\n\
                                /mode     — Show or change mode (build | plan)\n\
                                /mcp      — Show configured MCP server status\n\
                                /permissions [clear] — Show or clear always-allowed tools\n\
                                /resume [id] — Resume the most recent or selected session\n\
                                /session [status|list|resume|fork|open|new] — Manage durable sessions\n\
                                /compact  — Compact older complete turns now\n\
                                /goal     — Set, inspect, complete, or clear the active goal\n\
                                /loop [N|resume|status|stop] — Run or resume bounded autonomous work\n\
                                /init [path] — Initialize a .neenee/ config tree\n\
                                /clear    — Clear conversation history\n\
                                /exit     — Exit the program\n\
                                /help     — Show this message{}", custom_help)
                            ));
                        }
                        "/exit" => {
                            let _ = resp_tx.send(AgentResponse::Exit);
                        }
                        _ => {
                            let (name, arguments) = split_custom_command(&cmd);
                            let Some(command) = commands_for_task.get(name) else {
                                let _ = resp_tx.send(AgentResponse::Error(format!(
                                    "Unknown command: {}",
                                    parts[0]
                                )));
                                continue;
                            };
                            start_interactive_turn(
                                InteractiveTurnContext {
                                    agent: agent.clone(),
                                    history: history.clone(),
                                    tx: resp_tx.clone(),
                                    token_slot: ctt_clone.clone(),
                                    generation_counter: generation_clone.clone(),
                                    session: session.clone(),
                                    compaction_max_chars: config.compaction_max_chars,
                                    compaction_preserve_turns: config.compaction_preserve_turns,
                                    retry_max_attempts: config.provider_retry_max_attempts,
                                    retry_base_ms: config.provider_retry_base_ms,
                                    retry_max_ms: config.provider_retry_max_ms,
                                },
                                TurnInput {
                                    prompt: expand_command(command, arguments),
                                    hidden: false,
                                    display_prompt: Some(cmd),
                                },
                            )
                            .await;
                        }
                    }
                }
                AgentRequest::Chat(text) => {
                    start_interactive_turn(
                        InteractiveTurnContext {
                            agent: agent.clone(),
                            history: history.clone(),
                            tx: resp_tx.clone(),
                            token_slot: ctt_clone.clone(),
                            generation_counter: generation_clone.clone(),
                            session: session.clone(),
                            compaction_max_chars: config.compaction_max_chars,
                            compaction_preserve_turns: config.compaction_preserve_turns,
                            retry_max_attempts: config.provider_retry_max_attempts,
                            retry_base_ms: config.provider_retry_base_ms,
                            retry_max_ms: config.provider_retry_max_ms,
                        },
                        TurnInput {
                            prompt: text,
                            hidden: false,
                            display_prompt: None,
                        },
                    )
                    .await;
                }
            }
        }
    });

    // Start TUI in the main thread
    match start_tui(
        req_tx,
        resp_rx,
        initial_p_name,
        initial_m_name,
        input_history,
        restored_messages,
        custom_command_suggestions,
    )
    .await
    {
        Ok(history) => {
            let _ = Config::save_history(&history);
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

fn format_goal_status(goal: &Goal) -> String {
    let mut lines = vec![format!("Goal ({:?}): {}", goal.status, goal.objective)];
    for (index, item) in goal.checklist.iter().enumerate() {
        lines.push(format!(
            "{}. [{:?}] {}",
            index + 1,
            item.status,
            item.content
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::atomic::AtomicUsize;

    struct RetryOnceProvider(AtomicUsize);
    struct ToolThenRetryProvider(AtomicUsize);
    struct RetryReadTool;

    #[async_trait]
    impl Provider for RetryOnceProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("non-streaming path should not be used".to_string())
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }

        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<ProviderStreamEvent, String>>, String>
        {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(stream::iter(vec![
                    Ok(ProviderStreamEvent::TextDelta("partial".to_string())),
                    Err(neenee_core::retryable_error("rate limited", Some(1))),
                ])))
            } else {
                Ok(Box::pin(stream::iter(vec![Ok(
                    ProviderStreamEvent::TextDelta("done".to_string()),
                )])))
            }
        }
    }

    #[async_trait]
    impl Provider for ToolThenRetryProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("non-streaming path should not be used".to_string())
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }

        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<ProviderStreamEvent, String>>, String>
        {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(stream::iter(vec![Ok(
                    ProviderStreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call".to_string()),
                        name: Some("retry_read".to_string()),
                        arguments: "{}".to_string(),
                    },
                )])))
            } else {
                Ok(Box::pin(stream::iter(vec![Err(
                    neenee_core::retryable_error("upstream unavailable", None),
                )])))
            }
        }
    }

    #[async_trait]
    impl neenee_core::Tool for RetryReadTool {
        fn name(&self) -> &str {
            "retry_read"
        }

        fn description(&self) -> &str {
            "retry safety test"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn access(&self) -> neenee_core::ToolAccess {
            neenee_core::ToolAccess::ReadOnly
        }

        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("read".to_string())
        }
    }

    #[tokio::test]
    async fn proxy_provider_does_not_block_the_async_runtime() {
        let holder: Arc<RwLock<Arc<dyn Provider>>> = Arc::new(RwLock::new(Arc::new(MockProvider)));
        let proxy = ProxyProvider { holder };

        proxy.prepare_tools(&[]);
        let response = proxy.chat(Vec::new()).await.unwrap();

        assert!(response.content.contains("mock AI"));
    }

    #[test]
    fn context_overflow_detection_is_conservative() {
        assert!(is_context_overflow(
            "maximum context length exceeded for this model"
        ));
        assert!(is_context_overflow("too many tokens in request"));
        assert!(!is_context_overflow("network connection reset"));
    }

    #[test]
    fn goal_status_includes_structured_checklist() {
        let goal = Goal {
            objective: "ship".to_string(),
            status: GoalStatus::Active,
            checklist: vec![neenee_core::GoalChecklistItem {
                content: "verify".to_string(),
                status: neenee_core::GoalChecklistStatus::InProgress,
            }],
        };

        let status = format_goal_status(&goal);
        assert!(status.contains("ship"));
        assert!(status.contains("[InProgress] verify"));
    }

    #[tokio::test]
    async fn turn_retries_transient_provider_failure_before_tool_activity() {
        let directory =
            std::env::temp_dir().join(format!("neenee-retry-test-{}", uuid::Uuid::new_v4()));
        let session = Arc::new(SessionStore::for_path(directory.join("session.json")));
        let history = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let agent = Arc::new(Agent::new(
            Arc::new(RetryOnceProvider(AtomicUsize::new(0))),
            Vec::new(),
            AgentMode::Build,
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let completed = execute_turn(
            TurnContext {
                agent,
                history: history.clone(),
                tx,
                token: CancellationToken::new(),
                session,
                compaction_max_chars: 100_000,
                compaction_preserve_turns: 6,
                retry_max_attempts: 3,
                retry_base_ms: 1,
                retry_max_ms: 10,
            },
            TurnInput {
                prompt: "work".to_string(),
                hidden: false,
                display_prompt: None,
            },
        )
        .await
        .unwrap();

        assert!(!completed);
        assert!(history
            .lock()
            .await
            .iter()
            .any(|message| message.content == "done"));
        let responses = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let activities = responses
            .iter()
            .filter_map(|response| match response {
                AgentResponse::Activity(status) => Some(status.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(activities.starts_with(&["saving request", "preparing context"]));
        assert_eq!(
            activities
                .iter()
                .filter(|status| **status == "waiting for model")
                .count(),
            2
        );
        assert_eq!(activities.last(), Some(&"saving response"));
        assert!(responses.iter().any(|response| matches!(
            response,
            AgentResponse::RetryScheduled {
                attempt: 2,
                max_attempts: 3,
                ..
            }
        )));
        assert!(responses
            .iter()
            .any(|response| matches!(response, AgentResponse::StreamDiscard)));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn turn_does_not_retry_after_tool_activity() {
        let directory =
            std::env::temp_dir().join(format!("neenee-retry-tool-{}", uuid::Uuid::new_v4()));
        let session = Arc::new(SessionStore::for_path(directory.join("session.json")));
        let agent = Arc::new(Agent::new(
            Arc::new(ToolThenRetryProvider(AtomicUsize::new(0))),
            vec![Arc::new(RetryReadTool)],
            AgentMode::Build,
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let error = execute_turn(
            TurnContext {
                agent,
                history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                tx,
                token: CancellationToken::new(),
                session,
                compaction_max_chars: 100_000,
                compaction_preserve_turns: 6,
                retry_max_attempts: 4,
                retry_base_ms: 1,
                retry_max_ms: 10,
            },
            TurnInput {
                prompt: "work".to_string(),
                hidden: false,
                display_prompt: None,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error, "upstream unavailable");
        assert!(!std::iter::from_fn(|| rx.try_recv().ok())
            .any(|response| matches!(response, AgentResponse::RetryScheduled { .. })));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn retry_delay_honors_headers_and_exponential_bounds() {
        assert_eq!(retry_delay_ms(1, None, 1_000, 30_000), 1_000);
        assert_eq!(retry_delay_ms(3, None, 1_000, 30_000), 4_000);
        assert_eq!(retry_delay_ms(2, Some(45_000), 1_000, 30_000), 30_000);
    }
}
