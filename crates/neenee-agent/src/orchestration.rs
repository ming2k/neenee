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

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use futures::Stream;
use futures::stream::{BoxStream, StreamExt};
use serde::Serialize;
use tokio::sync::{RwLock as AsyncRwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::Agent;
use neenee_core::{
    AgentEvent, AgentRequest, AgentResponse, CronExpr, HarnessError, HarnessSnapshot, ImagePart,
    InjectionKind, InjectionOrigin, Message, NoticeKind, NoticeSeverity, NoticeSource,
    NoticeSurface, PURSUIT_COMPLETE_MARKER, Provider, ProviderStreamEvent, Pursuit, Role,
    TurnEvent,
};
use neenee_store::{
    RepeatStore,
    config::Config,
    session::{
        ContextProjectionCheckpoint, ContextProjectionResult, PursuitCheckpoint, SessionStore,
        UNCAPPED_ITERATIONS, estimate_chars, estimate_tokens, run_compaction,
    },
};

/// Wrap a session-scoped [`TurnEvent`] in the [`AgentResponse::Turn`]
/// envelope (ADR-0017). Every per-turn emitter routes through this so the
/// session id is attached uniformly, letting the TUI key transcript buffers
/// by `session_id` and dispatch primary vs `/btw` side events correctly.
pub fn turn(session_id: &str, event: TurnEvent) -> AgentResponse {
    AgentResponse::Turn {
        session_id: session_id.to_string(),
        event,
    }
}

pub struct ProxyProvider {
    pub holder: Arc<RwLock<Arc<dyn Provider>>>,
    /// Whether `/debug network` capture is armed. Read on every call so the
    /// toggle takes effect for the very next round-trip.
    debug_enabled: Arc<AtomicBool>,
    /// Dump directory while capture is on; `None` when off.
    debug_dir: Arc<std::sync::Mutex<Option<PathBuf>>>,
    /// Monotonic counter for unique filenames within the same millisecond.
    debug_seq: AtomicU64,
}

impl ProxyProvider {
    pub fn new(holder: Arc<RwLock<Arc<dyn Provider>>>) -> Self {
        Self {
            holder,
            debug_enabled: Arc::new(AtomicBool::new(false)),
            debug_dir: Arc::new(std::sync::Mutex::new(None)),
            debug_seq: AtomicU64::new(0),
        }
    }

    /// Resolve a capture record for the upcoming call, or `None` when capture
    /// is off. Clones the request messages once (only when armed) so the call
    /// can still move the originals into the inner provider.
    fn begin_capture(
        &self,
        provider: &str,
        model: &str,
        kind: &'static str,
        request: &[Message],
    ) -> Option<PendingCapture> {
        if !self.debug_enabled.load(Ordering::SeqCst) {
            return None;
        }
        let dir = self
            .debug_dir
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        Some(PendingCapture {
            provider: provider.to_string(),
            model: model.to_string(),
            kind,
            dir,
            request: request.to_vec(),
            seq: self.debug_seq.fetch_add(1, Ordering::SeqCst),
        })
    }
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

    fn set_debug_capture(&self, enabled: bool, dir: PathBuf) {
        self.debug_enabled.store(enabled, Ordering::SeqCst);
        *self
            .debug_dir
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = if enabled { Some(dir) } else { None };
    }

    fn debug_capture_enabled(&self) -> bool {
        self.debug_enabled.load(Ordering::SeqCst)
    }

    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let provider_id = p.provider_id();
        let model = p.model();
        let started = Instant::now();
        let capture = self.begin_capture(&provider_id, &model, "chat", &messages);
        let result = p.chat(messages).await;
        if let Some(capture) = capture {
            let item = match &result {
                Ok(message) => serde_json::json!({
                    "status": "ok",
                    "duration_ms": started.elapsed().as_millis() as u64,
                    "message": message,
                }),
                Err(error) => serde_json::json!({
                    "status": "error",
                    "duration_ms": started.elapsed().as_millis() as u64,
                    "error": error,
                }),
            };
            write_capture(&capture, &[item]);
        }
        result
    }
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let provider_id = p.provider_id();
        let model = p.model();
        let capture = self.begin_capture(&provider_id, &model, "stream_chat", &messages);
        let stream = p.stream_chat(messages).await;
        match (capture, stream) {
            (Some(capture), Err(error)) => {
                write_capture(
                    &capture,
                    &[serde_json::json!({ "status": "error", "error": error })],
                );
                Err(error)
            }
            (Some(capture), Ok(stream)) => Ok(CapturedStream {
                inner: stream,
                items: Vec::new(),
                capture,
            }
            .boxed()),
            (None, Ok(stream)) => Ok(stream),
            (None, Err(error)) => Err(error),
        }
    }
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        let p = self
            .holder
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let provider_id = p.provider_id();
        let model = p.model();
        let capture = self.begin_capture(&provider_id, &model, "stream_chat_events", &messages);
        let stream = p.stream_chat_events(messages).await;
        match (capture, stream) {
            (Some(capture), Err(error)) => {
                write_capture(
                    &capture,
                    &[serde_json::json!({ "status": "error", "error": error })],
                );
                Err(error)
            }
            (Some(capture), Ok(stream)) => Ok(CapturedStream {
                inner: stream,
                items: Vec::new(),
                capture,
            }
            .boxed()),
            (None, Ok(stream)) => Ok(stream),
            (None, Err(error)) => Err(error),
        }
    }
}

// ── /debug network capture ────────────────────────────────────────────

/// A queued capture record awaiting its response. Held across the inner call
/// (for `chat`) or inside a [`CapturedStream`] (for the streaming paths) and
/// flushed once the round-trip is complete.
struct PendingCapture {
    provider: String,
    model: String,
    kind: &'static str,
    dir: PathBuf,
    request: Vec<Message>,
    seq: u64,
}

/// Stream wrapper that tees every item into a buffer and flushes a single
/// capture file on drop — so one streaming round-trip yields one complete JSON
/// file, whether the stream ran to completion, errored, or was cancelled
/// mid-stream (a cancelled stream simply writes whatever was collected).
///
/// The wrapper is `Unpin`: its only pinned field (`inner: BoxStream`) is a
/// `Pin<Box<…>>`, which is itself `Unpin`, so `Pin::new(&mut self.inner)` is
/// sound. This keeps `poll_next` free of unsafe.
struct CapturedStream<S> {
    inner: S,
    items: Vec<serde_json::Value>,
    capture: PendingCapture,
}

impl<S> Stream for CapturedStream<S>
where
    S: Stream + Unpin,
    S::Item: Serialize,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(item)) => {
                this.items
                    .push(serde_json::to_value(&item).unwrap_or(serde_json::Value::Null));
                Poll::Ready(Some(item))
            }
            other => other,
        }
    }
}

impl<S> Drop for CapturedStream<S> {
    fn drop(&mut self) {
        write_capture(&self.capture, &self.items);
    }
}

/// Serialize one capture record and write it atomically to the dump directory.
/// Failures are logged and swallowed: debug capture must never break a real
/// turn. Files are owner-only (`0o600`) via `atomic_write_bytes` — request
/// messages can carry pasted secrets, the same privacy profile as `/export`.
fn write_capture(capture: &PendingCapture, items: &[serde_json::Value]) {
    let timestamp = chrono::Utc::now();
    let record = serde_json::json!({
        "timestamp": timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "provider": capture.provider,
        "model": capture.model,
        "kind": capture.kind,
        "request": { "messages": capture.request },
        "response": { "items": items },
    });
    let bytes = match serde_json::to_vec_pretty(&record) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "network capture serialize failed");
            return;
        }
    };
    let provider_slug = slug(&capture.provider);
    let model_slug = slug(&capture.model);
    let stamp = timestamp.format("%Y%m%d-%H%M%S%.3f");
    let file = capture.dir.join(format!(
        "{stamp}_{seq:04}_{provider_slug}_{model_slug}.json",
        seq = capture.seq,
    ));
    if let Err(error) = neenee_store::fsutil::atomic_write_bytes(&file, &bytes) {
        tracing::warn!(%error, file = %file.display(), "network capture write failed");
    }
}

/// Lowercase alnum/hyphen filename component, empty -> `"anon"`.
fn slug(value: &str) -> String {
    let mut out: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .map(|character| character.to_ascii_lowercase())
        .collect();
    if out.is_empty() {
        out.push_str("anon");
    }
    out
}

#[derive(Clone)]
pub struct ContextProjectionSettings {
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

impl ContextProjectionSettings {
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

/// Mid-turn model-context projection gate: prunes old tool results durably when
/// the active turn is approaching the model's context budget.
pub struct MidTurnPruneProjectionGate {
    pub session: Arc<SessionStore>,
    pub prune_protect_chars: usize,
}

#[async_trait]
impl neenee_core::ContextProjectionGate for MidTurnPruneProjectionGate {
    async fn project_context(&self, messages: Vec<Message>) -> Option<Vec<Message>> {
        let mut messages = messages;
        let outcome = neenee_core::prune_tool_results(
            &mut messages,
            self.prune_protect_chars,
            ContextProjectionSettings::PRUNE_MIN_RECLAIM_CHARS,
        )?;
        let after_chars = estimate_chars(&messages);
        let checkpoint = ContextProjectionCheckpoint {
            operation: neenee_store::session::ContextProjectionKind::Prune,
            archived_messages: outcome.originals.len(),
            active_messages: messages.len(),
            before_chars: after_chars + outcome.reclaimed_chars,
            after_chars,
        };
        let result = ContextProjectionResult {
            model_window: messages.clone(),
            archived_originals: outcome.originals,
            checkpoint,
        };
        if let Err(error) = self.session.commit_context_projection(result).await {
            tracing::warn!(?error, "mid-turn prune commit failed");
        }
        Some(messages)
    }
}

/// Emit the current harness snapshot (mode, pursuit, loop status, unattended)
/// to the UI.
pub fn send_harness_state(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    session_id: &str,
    agent: &Agent,
    loop_status: impl Into<String>,
) {
    let _ = tx.send(turn(
        session_id,
        TurnEvent::HarnessState(HarnessSnapshot {
            pursuit: agent.get_pursuit(),
            loop_status: loop_status.into(),
            unattended: agent.get_unattended(),
        }),
    ));
}

pub async fn refresh_agent_pursuit(agent: &Agent, session: &SessionStore) -> Option<Pursuit> {
    match session.pursuit().await {
        Some(pursuit) => {
            agent.set_pursuit(pursuit.clone());
            Some(pursuit)
        }
        None => {
            agent.clear_pursuit();
            None
        }
    }
}

pub fn emit_pursuit_updated(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    session_id: &str,
    pursuit: &Pursuit,
) {
    let _ = tx.send(turn(session_id, TurnEvent::PursuitUpdated(pursuit.clone())));
}

#[derive(Clone)]
pub struct TurnContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token: CancellationToken,
    pub session: Arc<SessionStore>,
    /// Session id this turn belongs to (ADR-0017). Tags every emitted
    /// [`TurnEvent`] so the TUI routes primary vs `/btw` side events correctly.
    pub session_id: String,
    pub projection: ContextProjectionSettings,
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
    /// Session id this turn belongs to (ADR-0017). Tags every emitted
    /// [`TurnEvent`] so the TUI routes primary vs `/btw` side events correctly.
    pub session_id: String,
    pub projection: ContextProjectionSettings,
    pub retry_max_attempts: usize,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

pub async fn start_interactive_turn(context: InteractiveTurnContext, input: TurnInput) {
    let token = CancellationToken::new();
    let generation = context.generation_counter.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(previous) = context.token_slot.write().await.replace(token.clone()) {
        context.agent.reject_pending_permissions();
        context.agent.reject_pending_inputs();
        let _ = context.tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }
    let _ = context.tx.send(turn(
        &context.session_id,
        TurnEvent::Activity("starting request".to_string()),
    ));

    tokio::spawn(async move {
        send_harness_state(&context.tx, &context.session_id, &context.agent, "running");
        let result = execute_turn(
            TurnContext {
                agent: context.agent.clone(),
                history: context.history,
                tx: context.tx.clone(),
                token: token.clone(),
                session: context.session,
                session_id: context.session_id.clone(),
                projection: context.projection,
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
                let _ = context.tx.send(turn(
                    &context.session_id,
                    TurnEvent::Text("... [Interrupted]".to_string()),
                ));
            }
            Err(error) if is_current => {
                let _ = context.tx.send(turn(
                    &context.session_id,
                    TurnEvent::Error(error.to_string()),
                ));
            }
            Err(_) => {}
        }
        let mut slot = context.token_slot.write().await;
        if context.generation_counter.load(Ordering::SeqCst) == generation {
            slot.take();
            send_harness_state(&context.tx, &context.session_id, &context.agent, "idle");
        }
    });
}

pub async fn execute_turn(
    context: TurnContext,
    mut input: TurnInput,
) -> Result<bool, HarnessError> {
    let TurnContext {
        agent,
        history,
        tx,
        token,
        session,
        session_id,
        projection,
        retry_max_attempts,
        retry_base_ms,
        retry_max_ms,
    } = context;
    // Bump the harness turn counter first thing so anything that reads it
    // during this turn (e.g. the `todo` / `todo_update` tools stamping
    // `updated_at_turn`) sees the new value. The TUI's stale detector
    // compares this against `TodoList::updated_at_turn`.
    agent.bump_turn();
    let _ = tx.send(turn(
        &session_id,
        TurnEvent::Activity("saving request".to_string()),
    ));

    // UserPromptSubmit hooks (ADR-0025): a hook may deny the prompt or prepend
    // context. Hidden control prompts (pursuit continuation, verify nudge) are
    // harness-internal and bypass the gate.
    if !input.hidden {
        match agent.fire_user_prompt_submit(&input.prompt).await {
            crate::hooks::UserPromptVerdict::Deny(reason) => {
                let _ = tx.send(turn(
                    &session_id,
                    TurnEvent::Text(format!("Prompt blocked by hook: {reason}")),
                ));
                return Ok(true);
            }
            crate::hooks::UserPromptVerdict::Prepend(context) => {
                input.prompt = format!("{context}\n\n{}", input.prompt);
            }
            crate::hooks::UserPromptVerdict::Allow => {}
        }
    }

    let admitted_session_id = session.id().await;
    // Build turn_history from the committed session history + the new
    // user message.  The user message is *not* pushed into the committed
    // `history` yet — if this turn fails (non-retryable provider error,
    // e.g. an image sent to a non-vision model), the history stays clean
    // and the next turn won't carry the poison.  On success the whole
    // `turn_history` (with the assistant reply and any tool results)
    // replaces `history` atomically (see the success path below).
    let mut turn_history = {
        let history = history.lock().await;
        let mut th = history.clone();
        th.push(if input.hidden {
            Message::injected(
                Role::User,
                input.prompt,
                InjectionOrigin::new(InjectionKind::HiddenTurnInput),
            )
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
        th
    };
    session.replace_messages(turn_history.clone()).await?;

    // Install the mid-turn save point (ADR-0035) so every tool-round boundary
    // durably appends its new messages to the session log. This is the fix for
    // the resume-after-crash gap: without it, a turn that ran side-effecting
    // tools and then crashed rewinds the transcript to the previous turn,
    // leaving it out of sync with the filesystem. The closure clones the
    // session `Arc` and the message slice (the `BoxFuture` is `'static`), then
    // delegates to `SessionStore::append_round`, which writes only the delta.
    {
        let session_for_round = Arc::clone(&session);
        agent.set_round_persist(Arc::new(move |messages: &[Message]| {
            let session = Arc::clone(&session_for_round);
            let snapshot = messages.to_vec();
            Box::pin(async move { session.append_round(&snapshot).await })
        }));
    }
    let _ = tx.send(turn(
        &session_id,
        TurnEvent::Activity("preparing context".to_string()),
    ));
    // Cheap tool-result pruning to relieve pressure before considering a full
    // compaction. Gated by the model-relative `prune_utilization` threshold
    // (ADR-0019) so it engages only once pressure crosses that fraction of the
    // window — not every turn — mirroring the mid-turn gate. Pruning also
    // self-limits to runs that reclaim meaningful space.
    if projection.prune && estimate_tokens(&turn_history) > projection.budget.prune_threshold_tokens
    {
        prune_and_commit(&mut turn_history, &session, &projection).await?;
    }
    if estimate_tokens(&turn_history) > projection.budget.compaction_threshold_tokens {
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::Activity("compacting context".to_string()),
        ));
        let extra = agent.fire_pre_compact().await;
        if let Some(checkpoint) = compact_turn_history(
            &mut turn_history,
            &session,
            &projection,
            Some(agent.provider.clone()),
            extra,
        )
        .await?
        {
            send_compaction(&tx, &session_id, &checkpoint);
        }
        agent.fire_post_compact().await;
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::Activity("preparing context".to_string()),
        ));
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
                relay_agent_event(&tx, &session_id, event, &streamed_for_run);
            })
            .await;

        let Err(error) = result else {
            break result;
        };
        if matches!(error, HarnessError::ContextOverflow(_))
            && !compacted_after_overflow
            && !tool_activity.load(Ordering::SeqCst)
        {
            let overflow_settings = ContextProjectionSettings {
                preserve_turns: projection.preserve_turns.max(1),
                ..projection.clone()
            };
            if compact_turn_history(
                &mut turn_history,
                &session,
                &overflow_settings,
                Some(agent.provider.clone()),
                Vec::new(),
            )
            .await?
            .is_some()
            {
                compacted_after_overflow = true;
                if streamed_text.swap(false, Ordering::SeqCst) {
                    let _ = tx.send(turn(&session_id, TurnEvent::StreamDiscard));
                }
                if let Some(checkpoint) = session.last_projection().await {
                    send_compaction(&tx, &session_id, &checkpoint);
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
        // Distinguish the two give-up reasons so the surfaced error explains
        // what happened instead of looking identical to a fresh failure that
        // was never retried. The tool-activity guard is intentional: once a
        // tool has run this turn the history may carry side effects we cannot
        // safely replay, so we stop rather than risk repeating them.
        if tool_activity.load(Ordering::SeqCst) {
            break Err(HarnessError::Other(format!(
                "{message}\n\nNot retried automatically because a tool already ran \
                 this turn and re-running could repeat its side effects. Resend \
                 the message to try again."
            )));
        }
        if attempt >= retry_limit {
            break Err(HarnessError::Other(format!(
                "{message}\n\nGave up after {retry_limit} attempt(s); the upstream \
                 service appears overloaded. Resend the message to try again, or \
                 raise `provider_retry_max_attempts` for more attempts."
            )));
        }
        if streamed_text.swap(false, Ordering::SeqCst) {
            let _ = tx.send(turn(&session_id, TurnEvent::StreamDiscard));
        }
        let delay_ms = retry_delay_ms(attempt, retry_after_ms, retry_base_ms, retry_max_ms);
        tracing::warn!(
            attempt = attempt + 1,
            max_attempts = retry_limit,
            delay_ms,
            "retrying after transient provider error"
        );
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::Notice(
                neenee_core::AgentNotice::new(
                    NoticeKind::ProviderRetry,
                    NoticeSeverity::Warning,
                    format!("Retrying provider request ({}/{retry_limit})", attempt + 1),
                    NoticeSource::Harness,
                )
                .with_body(format!(
                    "Waiting {}s before retrying: {}",
                    delay_ms.div_ceil(1_000),
                    public_retry_reason(&message)
                ))
                .with_surface(NoticeSurface::Toast),
            ),
        ));
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::RetryScheduled {
                attempt: attempt + 1,
                max_attempts: retry_limit,
                delay_ms,
                message,
            },
        ));
        tokio::select! {
            _ = token.cancelled() => return Err(HarnessError::Interrupted),
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
        }
    };
    if session.id().await != admitted_session_id {
        return Err(HarnessError::Interrupted);
    }
    let _ = tx.send(turn(
        &session_id,
        TurnEvent::Activity("saving response".to_string()),
    ));
    *history.lock().await = turn_history.clone();
    session.replace_messages(turn_history).await?;
    let outcome = result?;

    // Marker-based completion: if the model explicitly emitted the completion
    // marker and an active pursuit exists, mark it complete in the DB.
    let requested_completion = outcome.message.content.contains(PURSUIT_COMPLETE_MARKER);
    let mut completed = false;
    if requested_completion && agent.pursuit_can_complete() {
        match session.mark_pursuit_complete().await {
            Ok(Some(pursuit)) => {
                agent.set_pursuit(pursuit.clone());
                emit_pursuit_updated(&tx, &session_id, &pursuit);
                completed = true;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = tx.send(turn(
                    &session_id,
                    TurnEvent::Error(format!("Failed to mark pursuit complete: {error}")),
                ));
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
        let _ = tx.send(turn(&session_id, TurnEvent::Text(visible)));
    }
    if requested_completion && !completed {
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::Text(
                "Pursuit completion marker ignored: no active pursuit is set.".to_string(),
            ),
        ));
    }
    if completed {
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::Text("Pursuit completed.".to_string()),
        ));
    }

    // Mirror the unified task list so resume restores the sticky panel. The
    // value is compared against the session's current list to skip the write
    // (and avoid an event-log entry) on turns where nothing changed — the
    // common case.
    //
    // Auto-clear: once every item reaches a terminal status (completed or
    // cancelled), the task is finished and the list is dropped so a done list
    // does not linger in the panel (and the prompt) indefinitely. An empty
    // list is a no-op here.
    let agent_todos = agent.todos();
    if !agent_todos.items.is_empty() && agent_todos.is_all_done() {
        agent.clear_todos();
        let _ = tx.send(turn(
            &session_id,
            TurnEvent::TodosUpdated(neenee_core::TodoList::default()),
        ));
        if let Err(err) = session.set_todos(neenee_core::TodoList::default()).await {
            tracing::warn!(error = %err, "could not clear todos");
        }
    } else {
        let stored_todos = session.todos().await;
        if agent_todos != stored_todos
            && let Err(err) = session.set_todos(agent_todos).await
        {
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

fn public_retry_reason(message: &str) -> String {
    let first = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("transient provider error");
    const MAX_CHARS: usize = 96;
    if first.chars().count() <= MAX_CHARS {
        first.to_string()
    } else {
        let mut compact: String = first.chars().take(MAX_CHARS.saturating_sub(1)).collect();
        compact.push('…');
        compact
    }
}

pub fn relay_agent_event(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    session_id: &str,
    event: AgentEvent,
    streamed_text: &std::sync::atomic::AtomicBool,
) {
    let response = match event {
        AgentEvent::Notice(notice) => turn(session_id, TurnEvent::Notice(notice)),
        AgentEvent::ModelRequestStarted { tool_round } => {
            // Structured round signal first, so the Activity modal can show
            // `turn N · round M · waiting for model` with the round as a
            // first-class field rather than text-mining it out of the status
            // string. The bare status follows as the `Activity` below.
            let _ = tx.send(turn(
                session_id,
                TurnEvent::RoundStarted { round: tool_round },
            ));
            turn(
                session_id,
                TurnEvent::Activity("waiting for model".to_string()),
            )
        }
        AgentEvent::AssistantDelta { delta, start } => {
            if start {
                let _ = tx.send(turn(session_id, TurnEvent::StreamStart));
            }
            streamed_text.store(true, Ordering::SeqCst);
            turn(session_id, TurnEvent::StreamDelta(delta))
        }
        AgentEvent::AssistantEnd(content) => turn(
            session_id,
            TurnEvent::StreamEnd(
                content
                    .replace(PURSUIT_COMPLETE_MARKER, "")
                    .trim()
                    .to_string(),
            ),
        ),
        AgentEvent::AssistantDiscard => turn(session_id, TurnEvent::StreamDiscard),
        AgentEvent::ReasoningDelta { delta, start } => {
            if start {
                let _ = tx.send(turn(session_id, TurnEvent::StreamStart));
            }
            streamed_text.store(true, Ordering::SeqCst);
            turn(session_id, TurnEvent::StreamReasoningDelta(delta))
        }
        AgentEvent::ReasoningEnd(content) => {
            turn(session_id, TurnEvent::StreamReasoningEnd(content))
        }
        AgentEvent::ToolCall {
            id,
            name,
            arguments,
        } => turn(
            session_id,
            TurnEvent::ToolCall {
                id,
                name,
                arguments,
            },
        ),
        AgentEvent::ToolResult {
            id,
            name,
            output,
            structured,
            duration_ms,
        } => turn(
            session_id,
            TurnEvent::ToolResult {
                id,
                name,
                output,
                structured,
                duration_ms,
            },
        ),
        AgentEvent::ToolCancelled { id, name } => {
            turn(session_id, TurnEvent::ToolCancelled { id, name })
        }
        AgentEvent::ToolStream { id, stream } => {
            turn(session_id, TurnEvent::ToolStream { id, stream })
        }
        AgentEvent::PursuitUpdated(pursuit) => turn(session_id, TurnEvent::PursuitUpdated(pursuit)),
        AgentEvent::TodosUpdated(todos) => turn(session_id, TurnEvent::TodosUpdated(todos)),
        AgentEvent::UnattendedChanged(enabled) => {
            turn(session_id, TurnEvent::UnattendedChanged(enabled))
        }
        AgentEvent::SessionReview { alert } => {
            if !alert.trim().is_empty() {
                let _ = tx.send(turn(
                    session_id,
                    TurnEvent::Notice(
                        neenee_core::AgentNotice::new(
                            neenee_core::NoticeKind::ReviewAlert,
                            neenee_core::NoticeSeverity::Warning,
                            "Session review needs attention",
                            neenee_core::NoticeSource::Review,
                        )
                        .with_body(alert.clone())
                        .with_surface(neenee_core::NoticeSurface::Banner),
                    ),
                ));
            }
            turn(session_id, TurnEvent::SessionReview { alert })
        }
        AgentEvent::PermissionRequest(request) => {
            turn(session_id, TurnEvent::PermissionRequest(request))
        }
        AgentEvent::UserQuestionRequest(request) => {
            turn(session_id, TurnEvent::UserQuestionRequest(request))
        }
        AgentEvent::InputRequest(request) => turn(session_id, TurnEvent::InputRequest(request)),
        AgentEvent::Envoy {
            parent_call_id,
            event,
        } => turn(
            session_id,
            TurnEvent::Envoy {
                parent_call_id,
                event,
            },
        ),
    };
    let _ = tx.send(response);
}

pub async fn compact_turn_history(
    history: &mut Vec<Message>,
    session: &SessionStore,
    settings: &ContextProjectionSettings,
    provider: Option<Arc<dyn Provider>>,
    extra_context: Vec<String>,
) -> Result<Option<ContextProjectionCheckpoint>, String> {
    // Skip the model call entirely when summarization is disabled; the excerpt
    // fallback inside `run_compaction` still produces a checkpoint.
    let provider = if settings.summarize { provider } else { None };
    let Some(result) = run_compaction(
        history,
        settings.budget.target_tokens,
        settings.preserve_turns,
        provider,
        extra_context,
    )
    .await?
    else {
        return Ok(None);
    };
    let checkpoint = result.checkpoint.clone();
    session.commit_context_projection(result).await?;
    Ok(Some(checkpoint))
}

/// Prune old tool results in place and durably commit the change. Pruning is an
/// implicit model-context projection step: it keeps the conversation and the
/// `tool_call_id` chain intact (only stale tool *bodies* are cleared), so unlike
/// a summarizing compaction it does **not** surface a transcript notice — it
/// only records a durable checkpoint and a `debug` trace for observability.
pub async fn prune_and_commit(
    history: &mut [Message],
    session: &SessionStore,
    settings: &ContextProjectionSettings,
) -> Result<(), String> {
    let before_chars = estimate_chars(history);
    let Some(outcome) = neenee_core::prune_tool_results(
        history,
        settings.prune_protect_chars,
        ContextProjectionSettings::PRUNE_MIN_RECLAIM_CHARS,
    ) else {
        return Ok(());
    };
    let after_chars = estimate_chars(history);
    let checkpoint = ContextProjectionCheckpoint {
        operation: neenee_store::session::ContextProjectionKind::Prune,
        archived_messages: outcome.originals.len(),
        active_messages: history.len(),
        before_chars,
        after_chars,
    };
    tracing::debug!(
        pruned_tool_results = checkpoint.archived_messages,
        before_chars,
        after_chars,
        "pruned stale tool results"
    );
    session
        .commit_context_projection(ContextProjectionResult {
            model_window: history.to_owned(),
            archived_originals: outcome.originals,
            checkpoint,
        })
        .await
}

pub fn send_compaction(
    tx: &mpsc::UnboundedSender<AgentResponse>,
    session_id: &str,
    checkpoint: &ContextProjectionCheckpoint,
) {
    let _ = tx.send(turn(
        session_id,
        TurnEvent::Compacted {
            archived_messages: checkpoint.archived_messages,
            before_chars: checkpoint.before_chars,
            after_chars: checkpoint.after_chars,
        },
    ));
}

#[derive(Clone)]
pub struct PursuitContext {
    pub agent: Arc<Agent>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    pub token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    pub generation_counter: Arc<AtomicU64>,
    pub session: Arc<SessionStore>,
    /// Session id this pursuit belongs to (ADR-0017).
    pub session_id: String,
    pub projection: ContextProjectionSettings,
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
        context.agent.reject_pending_inputs();
        let _ = context.tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }

    send_harness_state(&context.tx, &context.session_id, &context.agent, "pursue");

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
                session_id: context.session_id.clone(),
                projection: context.projection.clone(),
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
                let _ = context.tx.send(turn(
                    &context.session_id,
                    TurnEvent::Text("Pursuit complete.".to_string()),
                ));
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
                let _ = context.tx.send(turn(
                    &context.session_id,
                    TurnEvent::Text(
                        "Pursuit stopped: safety cap reached or no completion signal.".to_string(),
                    ),
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
                let _ = context.tx.send(turn(
                    &context.session_id,
                    TurnEvent::Text("Pursuit interrupted.".to_string()),
                ));
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
                let _ = context.tx.send(turn(
                    &context.session_id,
                    TurnEvent::Error(error.to_string()),
                ));
            }
        }

        let mut slot = context.token_slot.write().await;
        if context.generation_counter.load(Ordering::SeqCst) == generation {
            slot.take();
            send_harness_state(&context.tx, &context.session_id, &context.agent, "idle");
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
