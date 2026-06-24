//! Turn-level orchestration policy on top of the `Agent` struct.
//!
//! `Agent` (in [`crate::agent`]) runs a single ReAct turn against a provider.
//! This module wraps every turn with the cross-cutting policy a frontend
//! cannot reasonably reimplement: context compaction (pre-turn and mid-turn
//! pruning), retry with exponential backoff, permission relay, the `/pursue`
//! stop-gate driver, and the `/repeat` cron scheduler.
//!
//! Frontends drive the harness through [`execute_turn`],
//! [`start_interactive_turn`], [`start_pursuit`], and
//! [`start_repeat_scheduler`]. They own only the UI-specific input path (slash commands for the CLI, menus/dialogs for a
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

use crate::Agent;
use neenee_core::{
    AgentEvent, AgentRequest, AgentResponse, CronExpr, HarnessError, HarnessSnapshot, ImagePart,
    Message, Provider, ProviderStreamEvent, Pursuit, PursuitService, RepeatStore, Role,
    PURSUIT_COMPLETE_MARKER,
};
use neenee_store::{
    config::Config,
    session::{
        estimate_chars, estimate_tokens, run_compaction, CompactionCheckpoint, CompactionDecision,
        CompactionHooks, CompactionResult, PursuitCheckpoint, SessionStore, UNCAPPED_ITERATIONS,
    },
};
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
    /// the live provider even after a mid-session `/provider` switch.
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
    /// Token thresholds resolved against the active model's context window.
    /// Pressure (estimated in tokens) is compared against these to decide when
    /// to prune and when to run a full summarizing compaction.
    pub budget: neenee_core::ContextBudget,
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

    /// Resolve settings for the active model's context window. `window_tokens`
    /// is the live model's context window (tokens); `0` means unknown and the
    /// policy's fallback window is substituted.
    pub fn from_config(config: &Config, window_tokens: usize) -> Self {
        Self {
            budget: config.compaction.resolve(window_tokens),
            preserve_turns: config.compaction_preserve_turns,
            summarize: config.compaction_summarize,
            prune: config.compaction_prune,
            prune_protect_chars: config.compaction_prune_protect_tokens
                * neenee_core::CHARS_PER_TOKEN,
        }
    }
}

/// Mid-turn context-relief gate: prunes old tool results durably when the
/// active turn is approaching the model's context budget.
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

/// Emit the current harness snapshot (mode, pursuit, loop status, auto-approve)
/// to the UI.
pub fn send_harness_state(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    agent: &Agent,
    loop_status: impl Into<String>,
) {
    let _ = tx.send(AgentResponse::HarnessState(HarnessSnapshot {
        mode: agent.get_mode(),
        pursuit: agent.get_pursuit(),
        loop_status: loop_status.into(),
        auto_approve: agent.get_auto_approve(),
    }));
}

pub async fn refresh_agent_pursuit(
    agent: &Agent,
    pursuit_service: &PursuitService,
    thread_id: &str,
) -> Option<Pursuit> {
    match pursuit_service.get_pursuit(thread_id).await {
        Ok(Some(db_pursuit)) => {
            let pursuit = db_pursuit;
            agent.set_pursuit(pursuit.clone());
            Some(pursuit)
        }
        Ok(None) => {
            agent.clear_pursuit();
            None
        }
        Err(_) => agent.get_pursuit(),
    }
}

pub fn emit_pursuit_updated(tx: &mpsc::UnboundedSender<AgentResponse>, pursuit: &Pursuit) {
    let _ = tx.send(AgentResponse::PursuitUpdated(pursuit.clone()));
}

#[derive(Clone)]
pub struct TurnContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token: CancellationToken,
    pub session: Arc<SessionStore>,
    pub pursuit_service: PursuitService,
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
    pub pursuit_service: PursuitService,
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
                pursuit_service: context.pursuit_service,
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
        pursuit_service,
        compaction,
        retry_max_attempts,
        retry_base_ms,
        retry_max_ms,
    } = context;
    // Bump the harness turn counter first thing so anything that reads it
    // during this turn (e.g. the `todo` / `todo_update` tools stamping
    // `updated_at_turn`) sees the new value. The TUI's stale detector
    // compares this against `TodoList::updated_at_turn`.
    agent.bump_turn();
    let _ = tx.send(AgentResponse::Activity("saving request".to_string()));
    let admitted_session_id = session.id().await;
    let thread_id = admitted_session_id.clone();
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
    if estimate_tokens(&turn_history) > compaction.budget.compaction_threshold_tokens {
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

    // Marker-based completion: if the model explicitly emitted the completion
    // marker and an active pursuit exists, mark it complete in the DB.
    let requested_completion = outcome.message.content.contains(PURSUIT_COMPLETE_MARKER);
    let mut completed = false;
    if requested_completion && agent.pursuit_can_complete() {
        match pursuit_service.mark_complete(&thread_id).await {
            Ok(Some(pursuit)) => {
                agent.set_pursuit(pursuit.clone());
                emit_pursuit_updated(&tx, &pursuit);
                completed = true;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = tx.send(AgentResponse::Error(format!(
                    "Failed to mark pursuit complete: {error}"
                )));
            }
        }
    } else if agent
        .get_pursuit()
        .is_some_and(|pursuit| pursuit.is_complete)
    {
        completed = true;
    }

    let visible = outcome
        .message
        .content
        .replace(PURSUIT_COMPLETE_MARKER, "")
        .trim()
        .to_string();
    if !visible.is_empty() && !streamed_text.load(Ordering::SeqCst) {
        let _ = tx.send(AgentResponse::Text(visible));
    }
    if requested_completion && !completed {
        let _ = tx.send(AgentResponse::Text(
            "Pursuit completion marker ignored: no active pursuit is set.".to_string(),
        ));
    }
    if completed {
        let _ = tx.send(AgentResponse::Text("Pursuit completed.".to_string()));
    }

    // Sync the agent's active plan path to the session so resume restores
    // the "you are implementing X" hint. The value is compared against the
    // session's current value to skip the write on turns where nothing
    // changed (the common case).
    let agent_plan = agent.active_plan_path();
    let stored_plan = session.active_plan_path().await;
    if agent_plan != stored_plan {
        if let Err(err) = session.set_active_plan_path(agent_plan).await {
            tracing::warn!(error = %err, "could not persist active plan path");
        }
    }

    // Mirror the unified task list so resume restores the sticky panel. The
    // value is compared against the session's current list to skip the write
    // (and avoid an event-log entry) on turns where nothing changed — the
    // common case.
    let agent_todos = agent.todos();
    let stored_todos = session.todos().await;
    if agent_todos != stored_todos {
        if let Err(err) = session.set_todos(agent_todos).await {
            tracing::warn!(error = %err, "could not persist todos");
        }
    }

    Ok(completed)
}

pub fn retry_delay_ms(
    attempt: usize,
    retry_after_ms: Option<u64>,
    base_ms: u64,
    max_ms: u64,
) -> u64 {
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
            // Structured round signal first, so the activity bar can show
            // `turn N · round M · waiting for model` with the round as a
            // first-class field rather than text-mining it out of the status
            // string. The bare status follows as the `Activity` below.
            let _ = tx.send(AgentResponse::RoundStarted { round: tool_round });
            AgentResponse::Activity("waiting for model".to_string())
        }
        AgentEvent::AssistantDelta { delta, start } => {
            if start {
                let _ = tx.send(AgentResponse::StreamStart);
            }
            streamed_text.store(true, Ordering::SeqCst);
            AgentResponse::StreamDelta(delta)
        }
        AgentEvent::AssistantEnd(content) => AgentResponse::StreamEnd(
            content
                .replace(PURSUIT_COMPLETE_MARKER, "")
                .trim()
                .to_string(),
        ),
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
        AgentEvent::PursuitUpdated(pursuit) => AgentResponse::PursuitUpdated(pursuit),
        AgentEvent::ModeChanged(mode) => AgentResponse::ModeChanged(mode),
        AgentEvent::TodosUpdated(todos) => AgentResponse::TodosUpdated(todos),
        AgentEvent::AutoApproveChanged(enabled) => AgentResponse::AutoApproveChanged(enabled),
        AgentEvent::SessionReview { alert } => AgentResponse::SessionReview { alert },
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
        settings.budget.target_tokens,
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

pub fn send_compaction(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    checkpoint: &CompactionCheckpoint,
) {
    let _ = tx.send(AgentResponse::Compacted {
        archived_messages: checkpoint.archived_messages,
        before_chars: checkpoint.before_chars,
        after_chars: checkpoint.after_chars,
    });
}

#[derive(Clone)]
pub struct PursuitContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    pub generation_counter: Arc<AtomicU64>,
    pub session: Arc<SessionStore>,
    pub pursuit_service: PursuitService,
    pub compaction: CompactionSettings,
    pub retry_max_attempts: usize,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

/// Run a pursuit: arm the stop-gate and execute a single agent turn.
///
/// The gate (`Agent::pursuit_continuation`) re-injects the condition and
/// forces additional rounds *within* the turn until the model signals
/// completion, the safety cap is hit, or the pursuit is interrupted — so a
/// single `execute_turn` here runs to completion instead of looping whole
/// turns (the old `/loop` model).
///
/// Terminates when:
/// - the model emits `PURSUIT_COMPLETE_MARKER` (`Ok(true)`);
/// - the stop-gate exhausts its safety cap (`Ok(false)` — disarmed without a
///   completion signal);
/// - the user interrupts (`Esc` or `/pursue stop`);
/// - a newer request supersedes this one (generation bump);
/// - a provider or tool pipeline error aborts the turn.
pub async fn start_pursuit(context: PursuitContext, condition: String) {
    let token = CancellationToken::new();
    let generation = context.generation_counter.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(previous) = context.token_slot.write().await.replace(token.clone()) {
        context.agent.reject_pending_permissions();
        let _ = context.tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }

    send_harness_state(&context.tx, &context.agent, "pursue");

    tokio::spawn(async move {
        // Arm the stop-gate so `execute_turn`'s turn loop keeps driving toward
        // the condition instead of ending on the first stop. The gate
        // self-disarms on cap/completion; we also disarm below as a backstop.
        context.agent.arm_pursuit();
        let _ = context
            .session
            .set_checkpoint(Some(PursuitCheckpoint {
                pursuit: condition.clone(),
                iteration: 1,
                max_iterations: UNCAPPED_ITERATIONS,
                status: "running".to_string(),
            }))
            .await;
        let prompt = format!(
            "Pursue this pursuit until it is fully achieved and verified:\n\
             {condition}\n\
             Work autonomously: inspect the current state, use tools, implement and verify \
             work. Do not stop at a plan. The harness keeps this turn going until the pursuit is \
             done, so keep making concrete progress. Emit {marker} only once the entire pursuit \
             is achieved and verified.",
            condition = condition,
            marker = PURSUIT_COMPLETE_MARKER,
        );
        let outcome = execute_turn(
            TurnContext {
                agent: context.agent.clone(),
                history: context.history.clone(),
                tx: context.tx.clone(),
                token: token.clone(),
                session: context.session.clone(),
                pursuit_service: context.pursuit_service.clone(),
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
        .await;
        context.agent.disarm_pursuit();
        match outcome {
            Ok(true) => {
                let _ = context
                    .session
                    .set_checkpoint(Some(PursuitCheckpoint {
                        pursuit: condition.clone(),
                        iteration: 1,
                        max_iterations: UNCAPPED_ITERATIONS,
                        status: "completed".to_string(),
                    }))
                    .await;
                let _ = context
                    .tx
                    .send(AgentResponse::Text("Pursuit complete.".to_string()));
            }
            Ok(false) => {
                // The stop-gate disarmed without a completion signal: either
                // the safety cap was reached or the model stopped without
                // emitting the marker.
                let _ = context
                    .session
                    .set_checkpoint(Some(PursuitCheckpoint {
                        pursuit: condition.clone(),
                        iteration: 1,
                        max_iterations: UNCAPPED_ITERATIONS,
                        status: "interrupted".to_string(),
                    }))
                    .await;
                let _ = context.tx.send(AgentResponse::Text(
                    "Pursuit stopped: safety cap reached or no completion signal.".to_string(),
                ));
            }
            Err(HarnessError::Interrupted) => {
                let _ = context
                    .session
                    .set_checkpoint(Some(PursuitCheckpoint {
                        pursuit: condition.clone(),
                        iteration: 1,
                        max_iterations: UNCAPPED_ITERATIONS,
                        status: "interrupted".to_string(),
                    }))
                    .await;
                let _ = context
                    .tx
                    .send(AgentResponse::Text("Pursuit interrupted.".to_string()));
            }
            Err(error) => {
                let _ = context
                    .session
                    .set_checkpoint(Some(PursuitCheckpoint {
                        pursuit: condition.clone(),
                        iteration: 1,
                        max_iterations: UNCAPPED_ITERATIONS,
                        status: "error".to_string(),
                    }))
                    .await;
                let _ = context.tx.send(AgentResponse::Error(error.to_string()));
            }
        }

        let mut slot = context.token_slot.write().await;
        if context.generation_counter.load(Ordering::SeqCst) == generation {
            slot.take();
            send_harness_state(&context.tx, &context.agent, "idle");
        }
    });
}

// ── /repeat scheduler ─────────────────────────────────────────────────

/// One scheduler tick: prune expired jobs, then dispatch every job whose
/// `next_fire` is due, advancing its schedule before enqueueing so a
/// slow turn cannot cause a double-fire.
pub async fn run_repeat_tick(
    store: &RepeatStore,
    tx: &mpsc::UnboundedSender<AgentRequest>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<usize, String> {
    let _ = store.prune_expired(now).await?;
    let due = store.due(now).await?;
    let mut dispatched = 0;
    for job in due {
        let next = match CronExpr::parse(&job.cron) {
            Ok(cron) => cron
                .next_fire(now)
                .unwrap_or(now + chrono::Duration::days(1)),
            Err(err) => {
                tracing::warn!(
                    "repeat job {} has unparseable cron '{}': {err}; skipping",
                    job.id,
                    job.cron
                );
                continue;
            }
        };
        store.mark_fired(&job.id, now, next).await?;
        let _ = tx.send(AgentRequest::Chat {
            text: job.prompt.clone(),
            images: Vec::new(),
        });
        dispatched += 1;
    }
    Ok(dispatched)
}

/// Spawn the durable `/repeat` scheduler. Every `tick_interval` it prunes
/// expired jobs and fires any that are due, dispatching each prompt as a
/// normal `AgentRequest::Chat` turn through `tx`.
pub fn start_repeat_scheduler(
    store: RepeatStore,
    tx: mpsc::UnboundedSender<AgentRequest>,
    tick_interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let now = chrono::Utc::now();
            if let Err(err) = run_repeat_tick(&store, &tx, now).await {
                tracing::warn!("repeat scheduler tick failed: {err}");
            }
        }
    })
}

#[cfg(test)]
mod repeat_tests {
    use super::*;
    use chrono::TimeZone;

    #[tokio::test]
    async fn tick_dispatches_and_advances_due_jobs() {
        let store = RepeatStore::open_in_memory_blocking().unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        // A job already due (next_fire == now).
        store.add("* * * * *", "run tests", now).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentRequest>();

        let dispatched = run_repeat_tick(&store, &tx, now).await.unwrap();
        assert_eq!(dispatched, 1);

        // The prompt was enqueued as a chat turn.
        match rx.recv().await {
            Some(AgentRequest::Chat { text, .. }) => assert_eq!(text, "run tests"),
            other => panic!("expected Chat, got {other:?}"),
        }
        // The job is no longer due at `now` (advanced to the next minute).
        let still_due = store.due(now).await.unwrap();
        assert!(still_due.is_empty());
    }

    #[tokio::test]
    async fn tick_skips_unparseable_cron() {
        let store = RepeatStore::open_in_memory_blocking().unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        // `add` does not validate cron, so a bogus expr can land here; the
        // tick must skip it rather than panic.
        store.add("not a cron", "p", now).await.unwrap();
        let (tx, _rx) = mpsc::unbounded_channel::<AgentRequest>();
        let dispatched = run_repeat_tick(&store, &tx, now).await.unwrap();
        assert_eq!(dispatched, 0);
    }
}
