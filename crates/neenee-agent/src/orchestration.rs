//! Turn-level orchestration policy on top of the `Agent` struct.
//!
//! `Agent` (in [`crate::agent`]) runs a single ReAct turn against a provider.
//! This module wraps every turn with the cross-cutting policy a frontend
//! cannot reasonably reimplement: context compaction (pre-turn and mid-turn
//! pruning), retry with exponential backoff, goal accounting, permission
//! relay, and the autonomous goal loop.
//!
//! Frontends drive the harness through [`execute_turn`],
//! [`start_interactive_turn`], and [`start_goal_loop`]. They own only the
//! UI-specific input path (slash commands for the CLI, menus/dialogs for a
//! future GUI); the actual turn machinery is shared here.
//!
//! All items are `pub` because they are assembled by the binary, which knows
//! the concrete provider/tool instances and the frontend's request channel.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};

use async_trait::async_trait;
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

use neenee_core::{
    AgentEvent, AgentResponse, Goal, GoalAccountingResult, GoalService, GoalStatus, HarnessError,
    HarnessSnapshot, ImagePart, Message, Provider, ProviderStreamEvent, Role, TurnTimer,
    GOAL_COMPLETE_MARKER,
};
use neenee_store::{
    session::{
        estimate_chars, run_compaction, CompactionCheckpoint, CompactionDecision,
        CompactionHooks, CompactionResult, LoopCheckpoint, SessionStore,
    },
    config::Config,
};
use crate::Agent;


pub struct ProxyProvider {
    pub holder: Arc<RwLock<Arc<dyn Provider>>>,
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

    /// Delegate to the currently active inner provider so attribution tracks
    /// the live provider even after a mid-session `/models` switch.
    fn provider_id(&self) -> String {
        self.holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .provider_id()
    }

    fn model(&self) -> String {
        self.holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .model()
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

#[derive(Clone)]
pub struct CompactionSettings {
    pub max_chars: usize,
    pub preserve_turns: usize,
    /// Use the active model to produce an anchored structured summary.
    pub summarize: bool,
    /// Enable cheap tool-result pruning (pre-turn and mid-turn).
    pub prune: bool,
    /// Character budget of the most recent tool results protected from pruning.
    pub prune_protect_chars: usize,
}

impl CompactionSettings {
    /// Mid-turn pruning only fires when it can reclaim at least this many chars,
    /// to avoid pruning churn for negligible gains.
    pub const PRUNE_MIN_RECLAIM_CHARS: usize = 8_000;

    /// Mid-turn relief trigger: prune between tool rounds once context pressure
    /// crosses this fraction of `max_chars` (`NUM/DEN` = 3/4), before the full
    /// pre-turn compaction threshold at `max_chars`.
    pub const MID_TURN_TRIGGER_NUM: usize = 3;
    pub const MID_TURN_TRIGGER_DEN: usize = 4;
}

impl From<&Config> for CompactionSettings {
    fn from(config: &Config) -> Self {
        Self {
            max_chars: config.compaction_max_chars,
            preserve_turns: config.compaction_preserve_turns,
            summarize: config.compaction_summarize,
            prune: config.compaction_prune,
            prune_protect_chars: config.compaction_prune_protect_chars,
        }
    }
}

/// Mid-turn context-relief gate: prunes old tool results durably when the

pub struct MidTurnCompactionGate {
    pub session: Arc<SessionStore>,
    pub prune_protect_chars: usize,
}

#[async_trait]
impl neenee_core::CompactionGate for MidTurnCompactionGate {
    async fn relieve_pressure(&self, messages: Vec<Message>) -> Option<Vec<Message>> {
        let mut messages = messages;
        let outcome = neenee_core::prune_tool_results(
            &mut messages,
            self.prune_protect_chars,
            CompactionSettings::PRUNE_MIN_RECLAIM_CHARS,
        )?;
        let after_chars = estimate_chars(&messages);
        let checkpoint = CompactionCheckpoint {
            archived_messages: outcome.originals.len(),
            active_messages: messages.len(),
            before_chars: after_chars + outcome.reclaimed_chars,
            after_chars,
        };
        let result = CompactionResult {
            active: messages.clone(),
            archived: outcome.originals,
            checkpoint,
        };
        if let Err(error) = self.session.commit_compaction(result).await {
            tracing::warn!(?error, "mid-turn prune commit failed");
        }
        Some(messages)
    }
}


pub struct RelayCompactionHooks {
    pub tx: mpsc::UnboundedSender<AgentResponse>,
}

#[async_trait]
impl CompactionHooks for RelayCompactionHooks {
    async fn pre_compact(&self, _messages: &[Message]) -> CompactionDecision {
        let _ = self
            .tx
            .send(AgentResponse::Activity("compacting context".to_string()));
        CompactionDecision::proceed()
    }

    async fn post_compact(&self, _checkpoint: &CompactionCheckpoint) {
        let _ = self
            .tx
            .send(AgentResponse::Activity("preparing context".to_string()));
    }
}

/// One-time migration for the pre-SQLite `harness_goal*` config fields.
/// Returns a `Goal` if the old config had one, so the caller can store it in

pub fn send_harness_state(
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

pub async fn refresh_agent_goal(
    agent: &Agent,
    goal_service: &GoalService,
    thread_id: &str,
) -> Option<Goal> {
    match goal_service.get_goal(thread_id).await {
        Ok(Some(db_goal)) => {
            let mut goal = db_goal;
            if let Some(mut current) = agent.get_goal() {
                goal.checklist = std::mem::take(&mut current.checklist);
            }
            agent.set_goal(goal.clone());
            Some(goal)
        }
        Ok(None) => {
            agent.clear_goal();
            None
        }
        Err(_) => agent.get_goal(),
    }
}

pub fn emit_goal_updated(tx: &mpsc::UnboundedSender<AgentResponse>, goal: &Goal) {
    let _ = tx.send(AgentResponse::GoalUpdated(goal.clone()));
}

#[derive(Clone)]
pub struct TurnContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token: CancellationToken,
    pub session: Arc<SessionStore>,
    pub goal_service: GoalService,
    pub compaction: CompactionSettings,
    pub retry_max_attempts: usize,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

pub struct TurnInput {
    pub prompt: String,
    pub hidden: bool,
    pub display_prompt: Option<String>,
    /// Inline images pasted into the prompt, attached to the user message.
    pub images: Vec<ImagePart>,
}

#[derive(Clone)]
pub struct InteractiveTurnContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    pub generation_counter: Arc<AtomicU64>,
    pub session: Arc<SessionStore>,
    pub goal_service: GoalService,
    pub compaction: CompactionSettings,
    pub retry_max_attempts: usize,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

pub async fn start_interactive_turn(context: InteractiveTurnContext, input: TurnInput) {
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
                goal_service: context.goal_service,
                compaction: context.compaction,
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
            Err(HarnessError::Interrupted) if is_current => {
                let _ = context
                    .tx
                    .send(AgentResponse::Text("... [Interrupted]".to_string()));
            }
            Err(error) if is_current => {
                let _ = context.tx.send(AgentResponse::Error(error.to_string()));
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

pub async fn execute_turn(context: TurnContext, input: TurnInput) -> Result<bool, HarnessError> {
    let TurnContext {
        agent,
        history,
        tx,
        token,
        session,
        goal_service,
        compaction,
        retry_max_attempts,
        retry_base_ms,
        retry_max_ms,
    } = context;
    let _ = tx.send(AgentResponse::Activity("saving request".to_string()));
    let admitted_session_id = session.id().await;
    let thread_id = admitted_session_id.clone();
    let timer = TurnTimer::new();
    let mut turn_history = {
        let mut history = history.lock().await;
        history.push(if input.hidden {
            Message::hidden(Role::User, input.prompt)
        } else {
            let message = Message::new(Role::User, input.prompt);
            let message = match input.display_prompt {
                Some(display) => message.with_display_content(display),
                None => message,
            };
            if input.images.is_empty() {
                message
            } else {
                message.with_images(input.images)
            }
        });
        history.clone()
    };
    session.replace_messages(turn_history.clone()).await?;
    let _ = tx.send(AgentResponse::Activity("preparing context".to_string()));
    // Cheap tool-result pruning to relieve pressure before considering a full
    // compaction. Only prunes when it can reclaim meaningful space.
    if compaction.prune {
        prune_and_commit(&mut turn_history, &session, &tx, &compaction).await?;
    }
    if estimate_chars(&turn_history) > compaction.max_chars {
        let hooks = RelayCompactionHooks { tx: tx.clone() };
        if let Some(checkpoint) = compact_turn_history(
            &mut turn_history,
            &session,
            &compaction,
            Some(agent.provider.clone()),
            &hooks,
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
        let result = agent
            .run_streaming_with_events(&mut turn_history, &token, |event| {
                if matches!(event, AgentEvent::ToolCall { .. }) {
                    activity_for_run.store(true, Ordering::SeqCst);
                }
                relay_agent_event(&tx, event, &streamed_for_run);
            })
            .await;

        let Err(error) = result else {
            break result;
        };
        if matches!(error, HarnessError::ContextOverflow(_))
            && !compacted_after_overflow
            && !tool_activity.load(Ordering::SeqCst)
        {
            let hooks = RelayCompactionHooks { tx: tx.clone() };
            let overflow_settings = CompactionSettings {
                preserve_turns: compaction.preserve_turns.max(1),
                ..compaction.clone()
            };
            if compact_turn_history(
                &mut turn_history,
                &session,
                &overflow_settings,
                Some(agent.provider.clone()),
                &hooks,
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

        let HarnessError::Retryable {
            message,
            retry_after_ms,
        } = error
        else {
            break Err(error);
        };
        if tool_activity.load(Ordering::SeqCst) || attempt >= retry_limit {
            break Err(HarnessError::Other(message));
        }
        if streamed_text.swap(false, Ordering::SeqCst) {
            let _ = tx.send(AgentResponse::StreamDiscard);
        }
        let delay_ms = retry_delay_ms(attempt, retry_after_ms, retry_base_ms, retry_max_ms);
        tracing::warn!(
            attempt = attempt + 1,
            max_attempts = retry_limit,
            delay_ms,
            "retrying after transient provider error"
        );
        let _ = tx.send(AgentResponse::RetryScheduled {
            attempt: attempt + 1,
            max_attempts: retry_limit,
            delay_ms,
            message,
        });
        tokio::select! {
            _ = token.cancelled() => return Err(HarnessError::Interrupted),
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
        }
    };
    if session.id().await != admitted_session_id {
        return Err(HarnessError::Interrupted);
    }
    let _ = tx.send(AgentResponse::Activity("saving response".to_string()));
    *history.lock().await = turn_history.clone();
    session.replace_messages(turn_history).await?;
    let outcome = result?;

    // Account the time and token cost of this turn against the persisted goal.
    match goal_service
        .account_turn(&thread_id, outcome.token_usage, timer.elapsed_seconds())
        .await
    {
        Ok(GoalAccountingResult::Updated(goal)) => {
            agent.set_goal(goal.clone());
            emit_goal_updated(&tx, &goal);
        }
        Ok(GoalAccountingResult::Unchanged) => {}
        Err(error) => {
            let _ = tx.send(AgentResponse::Error(format!(
                "Goal accounting failed: {error}"
            )));
        }
    }

    // Legacy /loop marker support: if the model explicitly emitted the completion
    // marker and the goal checklist allows completion, mark it complete in the DB.
    let requested_completion = outcome.message.content.contains(GOAL_COMPLETE_MARKER);
    let mut completed = false;
    if requested_completion && agent.goal_can_complete() {
        match goal_service.mark_complete(&thread_id).await {
            Ok(Some(goal)) => {
                agent.set_goal(goal.clone());
                emit_goal_updated(&tx, &goal);
                completed = true;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = tx.send(AgentResponse::Error(format!(
                    "Failed to mark goal complete: {error}"
                )));
            }
        }
    } else if agent
        .get_goal()
        .is_some_and(|goal| goal.status == GoalStatus::Complete)
    {
        completed = true;
    }

    let visible = outcome
        .message
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
        let _ = tx.send(AgentResponse::Text("Goal completed.".to_string()));
    }

    if agent
        .get_goal()
        .is_some_and(|goal| goal.status == GoalStatus::BudgetLimited)
    {
        let _ = tx.send(AgentResponse::Text(
            "Goal token budget exhausted. Use /goal budget <tokens> to increase it or /goal resume after reviewing.".to_string(),
        ));
    }

    Ok(completed)
}

pub fn retry_delay_ms(attempt: usize, retry_after_ms: Option<u64>, base_ms: u64, max_ms: u64) -> u64 {
    let exponent = attempt.saturating_sub(1).min(20) as u32;
    retry_after_ms
        .unwrap_or_else(|| base_ms.saturating_mul(2u64.saturating_pow(exponent)))
        .min(max_ms.max(1))
}

pub fn relay_agent_event(
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
            structured,
            duration_ms,
        } => AgentResponse::ToolResult {
            id,
            name,
            output,
            structured,
            duration_ms,
        },
        AgentEvent::ToolCancelled { id, name } => AgentResponse::ToolCancelled { id, name },
        AgentEvent::ToolStream { id, stream } => AgentResponse::ToolStream { id, stream },
        AgentEvent::GoalUpdated(goal) => AgentResponse::GoalUpdated(goal),
        AgentEvent::ModeChanged(mode) => AgentResponse::ModeChanged(mode),
        AgentEvent::PermissionRequest(request) => AgentResponse::PermissionRequest(request),
        AgentEvent::UserQuestionRequest(request) => AgentResponse::UserQuestionRequest(request),
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

pub async fn compact_turn_history(
    history: &mut Vec<Message>,
    session: &SessionStore,
    settings: &CompactionSettings,
    provider: Option<Arc<dyn Provider>>,
    hooks: &dyn CompactionHooks,
) -> Result<Option<CompactionCheckpoint>, String> {
    // Skip the model call entirely when summarization is disabled; the excerpt
    // fallback inside `run_compaction` still produces a checkpoint.
    let provider = if settings.summarize { provider } else { None };
    let Some(result) = run_compaction(
        history,
        settings.max_chars,
        settings.preserve_turns,
        provider,
        hooks,
    )
    .await?
    else {
        return Ok(None);
    };
    let checkpoint = result.checkpoint.clone();
    session.commit_compaction(result).await?;
    Ok(Some(checkpoint))
}

/// Prune old tool results in place and durably commit the change. Emits a
/// `Compacted` event only when pruning actually reclaims space.
pub async fn prune_and_commit(
    history: &mut [Message],
    session: &SessionStore,
    tx: &mpsc::UnboundedSender<AgentResponse>,
    settings: &CompactionSettings,
) -> Result<(), String> {
    let before_chars = estimate_chars(history);
    let Some(outcome) = neenee_core::prune_tool_results(
        history,
        settings.prune_protect_chars,
        CompactionSettings::PRUNE_MIN_RECLAIM_CHARS,
    ) else {
        return Ok(());
    };
    let after_chars = estimate_chars(history);
    let checkpoint = CompactionCheckpoint {
        archived_messages: outcome.originals.len(),
        active_messages: history.len(),
        before_chars,
        after_chars,
    };
    let result = CompactionResult {
        active: history.to_owned(),
        archived: outcome.originals,
        checkpoint: checkpoint.clone(),
    };
    session.commit_compaction(result).await?;
    send_compaction(tx, &checkpoint);
    Ok(())
}

pub fn send_compaction(tx: &mpsc::UnboundedSender<AgentResponse>, checkpoint: &CompactionCheckpoint) {
    let _ = tx.send(AgentResponse::Compacted {
        archived_messages: checkpoint.archived_messages,
        before_chars: checkpoint.before_chars,
        after_chars: checkpoint.after_chars,
    });
}

#[derive(Clone)]
pub struct LoopRunContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    pub generation_counter: Arc<AtomicU64>,
    pub session: Arc<SessionStore>,
    pub goal_service: GoalService,
    pub compaction: CompactionSettings,
    pub retry_max_attempts: usize,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

pub async fn start_goal_loop(
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
                    goal_service: context.goal_service.clone(),
                    compaction: context.compaction.clone(),
                    retry_max_attempts: context.retry_max_attempts,
                    retry_base_ms: context.retry_base_ms,
                    retry_max_ms: context.retry_max_ms,
                },
                TurnInput {
                    prompt,
                    hidden: true,
                    display_prompt: None,
                    images: Vec::new(),
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
                Err(HarnessError::Interrupted) => {
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
                    let _ = context.tx.send(AgentResponse::Error(error.to_string()));
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
