//! Terminal UI frontend: an in-house grid engine + crossterm app split into application
//! state ([`app`]), input mapping ([`input`]), the event/render loop
//! ([`event_loop`]), the semantic document model ([`document`]), and the
//! rendering engine ([`render`], with its `step`/`overlays`/`tools`
//! subtrees). [`start_tui`] is the entry point wired by `main`.

pub mod app;
pub mod clipboard;
pub mod clipboard_ops;
pub mod completion;
pub mod composer_attachments;
pub mod config;
pub mod document;
mod event_loop;
pub mod fuzzy;
pub mod input;
pub mod interaction;
pub mod layout;
pub mod providers;
pub mod question_model;
pub mod render;
pub mod selection;
pub mod step_interaction;
mod terminal;
mod transcript;
mod versioned;

pub(crate) use app::{ActivityTab, App, Modal, Recess};
pub(crate) use completion::{Completion, CompletionKind};
pub(crate) use providers::{
    PROVIDERS, ProviderPreset, RankedModel, model_display_name, models_filtered_from,
    provider_context_window,
};

use crossterm::{
    event::{
        DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use neenee_core::{
    AgentRequest, AgentResponse, HarnessSnapshot, Message, ParentStatus, PermissionRequest,
    ProviderPickerSnapshot, Pursuit, Role, SessionContextSnapshot, SessionOverview, TodoList,
    TurnEvent, UserQuestionRequest, mcp::McpConnectionStatus,
};
use neenee_tui::{Backend, Terminal};
use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    io,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, mpsc};

use crate::tui::document::{
    MessageKind, NoticeSeverity, TranscriptMessage, notice_severity_from_core,
};
use crate::tui::layout::LayoutMap;
use crate::tui::render::Theme;
use crate::tui::selection::{SelectionDrag, SelectionState};
use crate::tui::transcript::{finalize_streaming_reasoning, transcript_messages_from_core};

use neenee_store::session::SessionStore;

#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    mut rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
    initial_messages: Vec<Message>,
    custom_commands: Vec<(String, String)>,
    mcp_statuses: Vec<(String, McpConnectionStatus)>,
    tui_config: config::TuiConfig,
    session: Arc<SessionStore>,
) -> Result<Vec<String>, Box<dyn Error>> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Request the Kitty enhanced-keyboard protocol so modifier-bearing keys
    // that collide with legacy control bytes (notably Ctrl+M == Enter) are
    // reported distinctly. crossterm only emits the request when the terminal
    // advertises support, so this is a no-op elsewhere.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    // The neenee-tui engine owns its grid + diff + crossterm I/O directly. No
    // No ratatui, no WideHealBackend wrapper — the engine's retained grid writes
    // wide-glyph trailing cells with the glyph's own background at write time,
    // so ghost cells cannot occur regardless of terminal or multiplexer
    // (ADR-0038).
    let backend = Backend::new(stdout);
    let mut terminal = Terminal::new(backend);
    // Install the signal guard after the terminal enters raw mode + alt screen
    // so any later SIGTERM/SIGINT/SIGHUP restores it instead of stranding it.
    terminal::spawn_signal_guard();
    let tui_config = Arc::new(tui_config);
    let restored = transcript_messages_from_core(initial_messages, &tui_config);
    let messages = Arc::new(versioned::Versioned::new(restored));
    let messages_clone = messages.clone();
    // Stage 3 redraw signal: the listener flips this on every handled response
    // so the event loop knows shared state changed and a frame is due. Starts
    // `true` so the very first frame always renders.
    let dirty = Arc::new(AtomicBool::new(true));
    let dirty_clone = dirty.clone();
    // Stage 4 wakeup: the listener notifies this so the loop's `select!` wakes
    // immediately on a response instead of waiting out a poll interval.
    let dirty_notify = Arc::new(tokio::sync::Notify::new());
    let dirty_notify_clone = dirty_notify.clone();
    let should_quit = Arc::new(AtomicBool::new(false));
    let should_quit_clone = should_quit.clone();

    let current_provider = Arc::new(Mutex::new(initial_provider.clone()));
    let current_model = Arc::new(Mutex::new(initial_model.clone()));
    let cp_clone = current_provider.clone();
    let cm_clone = current_model.clone();

    let is_responding = Arc::new(AtomicBool::new(false));
    let ir_clone = is_responding.clone();
    let harness = Arc::new(Mutex::new(HarnessSnapshot {
        pursuit: None,
        loop_status: "idle".to_string(),
        unattended: false,
    }));
    let harness_clone = harness.clone();
    // Unified task list, mirrored from `AgentResponse::TodosUpdated`. Empty
    // (`None`) hides the panel.
    let todos: Arc<Mutex<Option<TodoList>>> = Arc::new(Mutex::new(None));
    let todos_clone = todos.clone();
    let turn_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let turn_count_clone = turn_count.clone();
    // Current tool round within the active turn. Reset to 0 at each turn
    // boundary and bumped from `AgentResponse::RoundStarted`. The activity bar
    // renders it as `round M` alongside the turn number.
    let current_round: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let current_round_clone = current_round.clone();
    // Stall alert level (consecutive read-only rounds). Bumped by future stall-
    // detection logic; reset at each turn boundary. Dormant until that logic
    // Session-review alert (ADR-0016). Updated when a `SessionReview`
    // response lands; cleared (empty) on turn reset so the activity bar's
    // `⚠ <alert>` segment clears between turns.
    let review_alert: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let review_alert_clone = review_alert.clone();
    // Wall-clock instant the current turn started. Stamped on a "running"
    // HarnessState so the activity bar can render a live `<elapsed>` segment.
    let turn_started_at: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    let turn_started_at_clone = turn_started_at.clone();
    let activity_status = Arc::new(Mutex::new(String::new()));
    let activity_clone = activity_status.clone();
    let pending_permission = Arc::new(Mutex::new(VecDeque::<PermissionRequest>::new()));
    let pending_permission_clone = pending_permission.clone();
    let pending_question = Arc::new(Mutex::new(VecDeque::<UserQuestionRequest>::new()));
    let pending_question_clone = pending_question.clone();
    // Full-duplex (ADR-0029): side-tables recording which subagent (by parent
    // tool-call id) surfaced a given permission / ask_user request, so the
    // modal's reply can be tagged with `parent_call_id` for down-routing.
    let subagent_permission_parent = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let subtask_permission_parent_clone = subagent_permission_parent.clone();
    let subagent_question_parent = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let subtask_question_parent_clone = subagent_question_parent.clone();
    let key_status = Arc::new(Mutex::new(HashMap::<String, bool>::new()));
    let key_status_clone = key_status.clone();
    let provider_picker = Arc::new(Mutex::new(ProviderPickerSnapshot::default()));
    let provider_picker_clone = provider_picker.clone();
    let sessions_overview = Arc::new(Mutex::new(Vec::<SessionOverview>::new()));
    let sessions_overview_clone = sessions_overview.clone();
    let open_sessions = Arc::new(AtomicBool::new(false));
    let open_sessions_clone = open_sessions.clone();
    // Latest session-context snapshot for the session modal (model / tools /
    // permissions / skills / mcp). Refreshed whenever the modal opens (the
    // event loop sends `QuerySessionContext`) and after any mutation the
    // harness applies (revoke / toggle). `None` until the first response lands.
    let session_context = Arc::new(Mutex::new(None::<SessionContextSnapshot>));
    let session_context_clone = session_context.clone();
    // Global tool-step density (true = Comfortable: new tool steps spawn
    // expanded). Shared with the response listener so steps created mid-turn
    // respect the user's last Ctrl+T choice (ADR-0001 Step 8).
    let tool_density = Arc::new(AtomicBool::new(false));
    let tool_density_clone = tool_density.clone();
    // TUI display config shared with the response listener so live tool steps
    // and reasoning traces honor the per-step-kind default expand state.
    let tui_config_clone = tui_config.clone();
    // `/btw` side-conversation shared state (ADR-0017). The side transcript
    // buffer, the parent-status mirror, and the one-shot view-transition
    // signal all cross the listener → loop boundary here.
    let side_messages = Arc::new(versioned::Versioned::new(Vec::<TranscriptMessage>::new()));
    let side_messages_clone = side_messages.clone();
    let parent_status = Arc::new(Mutex::new(ParentStatus::Idle));
    let parent_status_clone = parent_status.clone();
    let side_view_signal = Arc::new(Mutex::new(None::<event_loop::SideViewSignal>));
    let side_view_signal_clone = side_view_signal.clone();

    // `/serve` hot-attach tap (ADR-0037 §7). `None` until `/serve <port>`
    // activates it. The response listener clones each `AgentResponse` into the
    // broadcast sender while it is `Some`; the event loop writes to it when the
    // user types `/serve`.
    let serve_tap: Arc<tokio::sync::Mutex<Option<tokio::sync::broadcast::Sender<AgentResponse>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let serve_tap_for_listener = serve_tap.clone();
    let serve_tap_for_app = serve_tap.clone();

    // Spawn response listener
    tokio::spawn(async move {
        let mut reasoning_start: Option<std::time::Instant> = None;
        // Listener-local side routing key: the side `session_id` learned from
        // `SideViewOpened`. Kept here (not in `UiRuntime`) because only the
        // listener routes per-turn events; the loop reads the already-routed
        // `side_messages` buffer.
        let mut listener_side_id: Option<String> = None;
        while let Some(resp) = rx.recv().await {
            // Stage 3/4: any handled response can change shared state the loop
            // renders from, so signal a redraw (the flag) and wake the loop's
            // `select!` immediately (the notify). One pair here covers every
            // listener mutation (transcript, activity, todos, modals, …).
            dirty_clone.store(true, Ordering::Release);
            dirty_notify_clone.notify_one();
            // `/serve` hot-attach: clone the response into the broadcast
            // channel so WebSocket clients see the live stream. No-op when
            // serve is inactive (the lock holds None).
            if let Some(tx) = serve_tap_for_listener.lock().await.as_ref() {
                let _ = tx.send(resp.clone());
            }
            match resp {
                // ADR-0017: per-turn events arrive tagged with the session
                // they belong to. The listener routes each event to the side
                // buffer when its `session_id` matches the live side session,
                // and to the primary transcript otherwise. Permission and
                // user-question requests stay global so their modals surface
                // regardless of which view is focused.
                AgentResponse::Turn { session_id, event } => {
                    let routes_to_side = listener_side_id.as_deref() == Some(session_id.as_str());
                    // Select the transcript buffer for this event (ADR-0017):
                    // the side buffer when the event's `session_id` matches the
                    // live side session, the primary buffer otherwise. Global
                    // responding/activity/harness state below is gated on
                    // `!routes_to_side` so a concurrent side turn never
                    // clobbers the primary view's chrome; the side view reads
                    // its own buffer + the parent-status banner instead.
                    // Permission and user-question requests stay global
                    // regardless of origin so their modals always surface.
                    let buf = if routes_to_side {
                        &side_messages_clone
                    } else {
                        &messages_clone
                    };
                    match event {
                        TurnEvent::Notice(notice) => {
                            let mut msgs = buf.write().await;
                            push_core_notice(&mut msgs, &notice);
                        }
                        TurnEvent::Text(t) => {
                            let (provider, model) =
                                event_loop::attribution(&cp_clone, &cm_clone).await;
                            let mut msgs = buf.write().await;
                            msgs.push(
                                TranscriptMessage::new(Role::Assistant, t)
                                    .with_attribution(provider, model),
                            );
                            if !routes_to_side {
                                ir_clone.store(false, Ordering::SeqCst);
                                activity_clone.lock().await.clear();
                            }
                        }
                        TurnEvent::Activity(status) => {
                            if !routes_to_side {
                                *activity_clone.lock().await = status;
                                ir_clone.store(true, Ordering::SeqCst);
                            }
                        }
                        TurnEvent::RoundStarted { round } => {
                            if !routes_to_side {
                                // 1-indexed for display: tool_round 0 is the turn's
                                // first model request, shown as `round 1`.
                                *current_round_clone.lock().await = round as u64 + 1;
                            }
                        }
                        TurnEvent::StreamStart => {
                            let (provider, model) =
                                event_loop::attribution(&cp_clone, &cm_clone).await;
                            let mut msgs = buf.write().await;
                            msgs.push(
                                TranscriptMessage::new(Role::Assistant, "")
                                    .with_attribution(provider, model),
                            );
                            if !routes_to_side {
                                ir_clone.store(true, Ordering::SeqCst);
                                *activity_clone.lock().await = "responding".to_string();
                            }
                        }
                        TurnEvent::StreamDelta(delta) => {
                            let mut msgs = buf.write().await;
                            if let Some(last) = msgs.last_mut() {
                                last.push_stream(&delta);
                            }
                        }
                        TurnEvent::StreamEnd(final_content) => {
                            if !routes_to_side {
                                ir_clone.store(true, Ordering::SeqCst);
                                *activity_clone.lock().await = "finalizing response".to_string();
                            }
                            let mut msgs = buf.write().await;
                            if let Some(last) = msgs.last_mut() {
                                last.raw = final_content;
                                last.reparse();
                            }
                        }
                        TurnEvent::StreamDiscard => {
                            let mut msgs = buf.write().await;
                            if msgs
                                .last()
                                .is_some_and(|message| message.role == Role::Assistant)
                            {
                                msgs.pop();
                            }
                        }
                        TurnEvent::StreamReasoningDelta(delta) => {
                            let mut msgs = buf.write().await;
                            if let Some(last) =
                                msgs.last_mut().filter(|message| message.is_thinking())
                            {
                                last.push_stream(&delta);
                                if let MessageKind::Thinking { content, .. } = &mut last.kind {
                                    content.push_str(&delta);
                                }
                            } else {
                                // StreamStart inserts an empty assistant placeholder before
                                // the first reasoning delta. Reasoning renders as its own
                                // reasoning trace, so that placeholder is never used and only
                                // leaves an extra blank line between the user message and the
                                // reasoning header. Drop it before creating the reasoning trace
                                // so restored history and live reasoning have identical
                                // spacing.
                                if msgs
                                    .last()
                                    .is_some_and(|m| m.role == Role::Assistant && m.raw.is_empty())
                                {
                                    msgs.pop();
                                }
                                let (provider, model) =
                                    event_loop::attribution(&cp_clone, &cm_clone).await;
                                let mut thinking = TranscriptMessage::thinking(delta)
                                    .with_attribution(provider, model);
                                // A reasoning trace's default disclosure honors the
                                // `[tui.default_expanded] thinking` config (collapsed by
                                // default). On completion the transition leaves it as-is
                                // (no auto-collapse), so the user keeps what they were
                                // reading.
                                thinking.set_thinking_expanded(config::thinking_default_expanded(
                                    &tui_config_clone,
                                ));
                                msgs.push(thinking);
                                reasoning_start = Some(std::time::Instant::now());
                            }
                        }
                        TurnEvent::StreamReasoningEnd(content) => {
                            let duration_ms = reasoning_start
                                .take()
                                .map(|started| started.elapsed().as_millis() as u64);
                            let mut msgs = buf.write().await;
                            // The round closes with `AssistantEnd` *before* `ReasoningEnd`
                            // (see golden_reasoning_precedes_text_in_the_same_round), so by
                            // the time this arrives the assistant's text message is usually
                            // the literal last message. Scan backward for the most recent
                            // Thinking message that is still streaming (`duration_ms: None`)
                            // instead of relying on it being last — otherwise the trace's
                            // duration never gets stamped and the spinner runs forever.
                            let target = msgs.iter_mut().rfind(|message| {
                                matches!(
                                    &message.kind,
                                    MessageKind::Thinking {
                                        duration_ms: None,
                                        ..
                                    }
                                )
                            });
                            if let Some(last) = target {
                                last.raw = content.clone();
                                last.reparse();
                                if let MessageKind::Thinking {
                                    content: current,
                                    duration_ms: d,
                                    ..
                                } = &mut last.kind
                                {
                                    *current = content;
                                    if d.is_none() {
                                        *d = Some(duration_ms.unwrap_or(0));
                                    }
                                }
                            }
                        }
                        TurnEvent::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            if !routes_to_side {
                                *activity_clone.lock().await =
                                    event_loop::tool_activity_status(&name).to_string();
                            }
                            let (provider, model) =
                                event_loop::attribution(&cp_clone, &cm_clone).await;
                            let mut msgs = buf.write().await;
                            // A tool step starts collapsed: there's no result to show
                            // yet. The lifecycle-aware default (see `step_interaction`)
                            // expands it on completion — Ok follows per-tool density,
                            // Failed/Denied force-expand to surface the error.
                            let message = TranscriptMessage::tool_step(id, name, arguments)
                                .with_attribution(provider, model);
                            msgs.push(message);
                            if !routes_to_side {
                                ir_clone.store(true, Ordering::SeqCst);
                            }
                        }
                        TurnEvent::ToolResult {
                            id,
                            name,
                            output,
                            structured,
                            duration_ms,
                        } => {
                            if !routes_to_side {
                                *activity_clone.lock().await = "thinking".to_string();
                            }
                            let (provider, model) =
                                event_loop::attribution(&cp_clone, &cm_clone).await;
                            let density = tool_density_clone.load(Ordering::SeqCst);
                            let mut msgs = buf.write().await;
                            let mut finished = false;
                            for existing in msgs.iter_mut() {
                                if existing.finish_tool_step(
                                    &id,
                                    output.clone(),
                                    structured.clone(),
                                    duration_ms,
                                ) {
                                    // Apply the lifecycle-aware default disclosure: Ok
                                    // follows per-tool density, Failed/Denied force-
                                    // expand to surface the error. Respects any user
                                    // pin via the system setter.
                                    if let Some(status) = existing.tool_step_status() {
                                        let default = step_interaction::default_tool_expanded(
                                            status,
                                            &name,
                                            &tui_config_clone,
                                            density,
                                        );
                                        existing.set_tool_step_expanded(default);
                                    }
                                    finished = true;
                                    break;
                                }
                            }
                            if !finished {
                                // No matching in-flight call (e.g. turn restored from
                                // history): synthesize a finished step with its default
                                // disclosure applied directly.
                                let mut message =
                                    TranscriptMessage::tool_step(id.clone(), name.clone(), "{}")
                                        .with_attribution(provider, model);
                                message.finish_tool_step(&id, output, structured, duration_ms);
                                if let Some(status) = message.tool_step_status() {
                                    let default = step_interaction::default_tool_expanded(
                                        status,
                                        &name,
                                        &tui_config_clone,
                                        density,
                                    );
                                    message.set_tool_step_expanded(default);
                                }
                                msgs.push(message);
                            }
                        }
                        TurnEvent::ToolCancelled { id, .. } => {
                            // Convergence: an in-flight call was aborted by an
                            // interrupt. Flip its step (and any nested subagent
                            // children) to Cancelled so it never stays "running".
                            let mut msgs = buf.write().await;
                            let mut cancelled = false;
                            for message in msgs.iter_mut() {
                                if message.cancel_tool_step(&id) {
                                    // Cancelled reads as inert → collapse (respecting
                                    // any user pin via the system setter).
                                    message.set_tool_step_expanded(false);
                                    cancelled = true;
                                    break;
                                }
                            }
                            if !cancelled {
                                // The ToolCall event may have been dropped with the
                                // aborted turn; synthesize a minimal cancelled step so
                                // the user still sees the call was abandoned.
                                let mut message =
                                    TranscriptMessage::tool_step(id.clone(), "tool", "{}");
                                message.cancel_tool_step(&id);
                                message.set_tool_step_expanded(false);
                                msgs.push(message);
                            }
                        }
                        TurnEvent::ToolStream { id, stream } => {
                            // Live partial output from a running tool (e.g. bash
                            // stdout). Accumulate into the running step so it updates
                            // in place instead of freezing on a spinner.
                            let mut msgs = buf.write().await;
                            if !msgs
                                .iter_mut()
                                .any(|message| message.push_tool_stream(&id, &stream))
                            {
                                // Unknown id: drop silently — the matching ToolCall may
                                // have been dropped with an aborted turn.
                            }
                        }
                        TurnEvent::SubAgent {
                            parent_call_id,
                            event,
                        } => {
                            // Full-duplex (ADR-0029): a subagent's permission broker
                            // or `ask_user` request bubbles up nested under this
                            // `parent_call_id`. Surface it in the SAME modal the
                            // top-level path uses (so the user answers it inline) and
                            // record the parent so the reply gets tagged for
                            // down-routing into the child. Falls through to the nested
                            // transcript rendering below for the ordinary
                            // stream/tool-call events.
                            match &event {
                                neenee_core::SubagentEvent::PermissionRequest(req) => {
                                    subtask_permission_parent_clone
                                        .lock()
                                        .await
                                        .insert(req.id.clone(), parent_call_id.clone());
                                    pending_permission_clone.lock().await.push_back(req.clone());
                                    if !routes_to_side {
                                        *activity_clone.lock().await =
                                            "awaiting permission".to_string();
                                        ir_clone.store(true, Ordering::SeqCst);
                                    }
                                }
                                neenee_core::SubagentEvent::UserQuestionRequest(req) => {
                                    subtask_question_parent_clone
                                        .lock()
                                        .await
                                        .insert(req.id.clone(), parent_call_id.clone());
                                    pending_question_clone.lock().await.push_back(req.clone());
                                    if !routes_to_side {
                                        *activity_clone.lock().await =
                                            "awaiting user input".to_string();
                                        ir_clone.store(true, Ordering::SeqCst);
                                    }
                                }
                                _ => {}
                            }
                            let mut msgs = buf.write().await;
                            if let Some(message) = msgs
                        .iter_mut()
                        .find(|m| m.is_tool_step() && matches!(&m.kind, crate::tui::document::MessageKind::ToolStep { id, .. } if id == &parent_call_id))
                    {
                        message.push_subagent_event(&event);
                    }
                        }
                        TurnEvent::PermissionRequest(request) => {
                            // A single model response can carry several write tool
                            // calls, each emitting its own request before blocking on
                            // its reply. Queue them FIFO so none is lost; the UI shows
                            // one sheet at a time and hands off as each is resolved.
                            // Stays global regardless of session so the modal always
                            // surfaces (ADR-0017: the side runs unattended, so in
                            // practice only the primary ever reaches here).
                            pending_permission_clone.lock().await.push_back(request);
                            if !routes_to_side {
                                *activity_clone.lock().await = "awaiting permission".to_string();
                                ir_clone.store(true, Ordering::SeqCst);
                            }
                        }
                        TurnEvent::UserQuestionRequest(request) => {
                            pending_question_clone.lock().await.push_back(request);
                            if !routes_to_side {
                                *activity_clone.lock().await = "awaiting user input".to_string();
                                ir_clone.store(true, Ordering::SeqCst);
                            }
                        }
                        TurnEvent::Compacted {
                            archived_messages,
                            before_chars,
                            after_chars,
                        } => {
                            let mut msgs = buf.write().await;
                            push_local_notice(
                                &mut msgs,
                                NoticeSeverity::Info,
                                format!(
                                    "Compacted {} messages: {} -> {} chars.",
                                    archived_messages, before_chars, after_chars
                                ),
                            );
                        }
                        TurnEvent::HarnessState(snapshot) => {
                            let running = snapshot.loop_status != "idle";
                            if !routes_to_side {
                                *harness_clone.lock().await = snapshot;
                                // Each "running" HarnessState marks the start of a new
                                // turn; bump the local turn counter mirror so the plan
                                // panel's stale detector has a frame-current value
                                // without needing a dedicated event channel. This is
                                // approximate (one bump per turn start) which matches
                                // `Agent::bump_turn`'s semantics in the harness.
                                if running {
                                    let mut tc = turn_count_clone.lock().await;
                                    *tc = tc.saturating_add(1);
                                    // A new turn resets the round counter; it stays 0
                                    // until the first `RoundStarted` of the turn lands.
                                    *current_round_clone.lock().await = 0;
                                    // Reset the review alert and stamp the turn timer so the
                                    // activity bar can render a live `<elapsed>` segment.
                                    *review_alert_clone.lock().await = String::new();
                                    *turn_started_at_clone.lock().await =
                                        Some(std::time::Instant::now());
                                }
                                ir_clone.store(running, Ordering::SeqCst);
                                if !running {
                                    activity_clone.lock().await.clear();
                                    *current_round_clone.lock().await = 0;
                                    *review_alert_clone.lock().await = String::new();
                                    *turn_started_at_clone.lock().await = None;
                                }
                            }
                            // A harness state change is always a turn boundary
                            // (idle at the end of a turn, "running"/"loop N/M" at the
                            // start of a new one). If the previous turn ended mid-
                            // reasoning — e.g. the user interrupted, the provider
                            // errored, or a fresh turn superseded a still-streaming
                            // one — `StreamReasoningEnd` never arrives, so the
                            // in-flight Thinking message keeps `duration_ms: None`.
                            // That is exactly the state the renderer uses to decide
                            // the reasoning marker is "running" and should keep
                            // breathing its spinner, which would flash forever after
                            // an interrupt. Freeze any such orphaned trace by
                            // stamping its elapsed time (or 0 if the start instant
                            // was already consumed) so the spinner stops.
                            let duration_ms = reasoning_start
                                .take()
                                .map(|started| started.elapsed().as_millis() as u64);
                            let mut msgs = buf.write().await;
                            finalize_streaming_reasoning(&mut msgs, duration_ms);
                        }
                        TurnEvent::PursuitUpdated(pursuit) => {
                            let prev = if !routes_to_side {
                                harness_clone.lock().await.pursuit.clone()
                            } else {
                                None
                            };
                            if let Some(text) = describe_pursuit_change(prev.as_ref(), &pursuit) {
                                let mut msgs = buf.write().await;
                                push_local_notice(&mut msgs, NoticeSeverity::Info, text);
                            }
                            if !routes_to_side {
                                harness_clone.lock().await.pursuit = Some(pursuit);
                            }
                        }
                        TurnEvent::PursuitCleared => {
                            // Non-gated mirror: null the snapshot's pursuit
                            // field *without* flushing the activity cell. This
                            // is the fix for the activity-bar-flicker bug —
                            // `/pursue clear` used to refresh the field via a
                            // spurious `HarnessState("idle")`, which the
                            // HarnessState handler treats as a turn-end
                            // signal and uses to clear the live activity
                            // status, momentarily hiding the bar mid-turn.
                            if !routes_to_side {
                                harness_clone.lock().await.pursuit = None;
                            }
                        }
                        TurnEvent::TodosUpdated(list) => {
                            if !routes_to_side {
                                *todos_clone.lock().await = Some(list);
                            }
                        }
                        TurnEvent::UnattendedChanged(enabled) => {
                            if !routes_to_side {
                                harness_clone.lock().await.unattended = enabled;
                            }
                        }
                        TurnEvent::RetryScheduled {
                            attempt,
                            max_attempts,
                            delay_ms,
                            message,
                        } => {
                            if !routes_to_side {
                                let seconds = delay_ms.div_ceil(1_000);
                                *activity_clone.lock().await = format!(
                                    "retry {}/{} in {}s · {}",
                                    attempt,
                                    max_attempts,
                                    seconds,
                                    event_loop::compact_retry_reason(&message)
                                );
                                ir_clone.store(true, Ordering::SeqCst);
                            }
                        }
                        TurnEvent::Error(e) => {
                            let mut msgs = buf.write().await;
                            push_local_notice(&mut msgs, NoticeSeverity::Error, e);
                            if !routes_to_side {
                                ir_clone.store(false, Ordering::SeqCst);
                                activity_clone.lock().await.clear();
                            }
                        }
                        TurnEvent::SessionReview { alert } => {
                            if !routes_to_side {
                                // Mirror the latest review verdict into the runtime cell
                                // so the activity bar's `⚠ <alert>` segment shows the
                                // diagnostic's summary (or clears it when `alert` is
                                // empty — a healthy review). The frame loop copies this
                                // into `App::review_alert`, which `draw_activity_bar`
                                // reads.
                                *review_alert_clone.lock().await = alert;
                            }
                        }
                    } // end inner `match event`
                }
                AgentResponse::ParentStatus(status) => {
                    // ADR-0017: primary-session status for the `/btw` side
                    // banner. Mirrored into `App::parent_status` each frame.
                    *parent_status_clone.lock().await = status;
                }
                AgentResponse::SideViewOpened { side_id, .. } => {
                    // ADR-0017: enter the side view. Record the routing key so
                    // subsequent per-turn events stream into the side buffer,
                    // and queue the view transition for the event loop.
                    listener_side_id = Some(side_id.clone());
                    side_messages_clone.write().await.clear();
                    *side_view_signal_clone.lock().await =
                        Some(event_loop::SideViewSignal::Opened { side_id });
                }
                AgentResponse::SideViewClosed => {
                    // ADR-0017: leave the side view. Drop the routing key so
                    // events route back to the primary buffer.
                    listener_side_id = None;
                    *side_view_signal_clone.lock().await = Some(event_loop::SideViewSignal::Closed);
                }
                AgentResponse::PermissionsCleared => {
                    pending_permission_clone.lock().await.clear();
                    activity_clone.lock().await.clear();
                }
                AgentResponse::ProviderKeys(status) => {
                    *key_status_clone.lock().await = status.into_iter().collect();
                }
                AgentResponse::ProviderPicker(snapshot) => {
                    *provider_picker_clone.lock().await = snapshot;
                }
                AgentResponse::ConversationCleared => {
                    messages_clone.write().await.clear();
                }
                AgentResponse::ConversationReplaced(messages) => {
                    *messages_clone.write().await =
                        transcript_messages_from_core(messages, &tui_config_clone);
                }
                AgentResponse::SessionsOverview(sessions) => {
                    *sessions_overview_clone.lock().await = sessions;
                    open_sessions_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::SessionContext(snapshot) => {
                    *session_context_clone.lock().await = Some(snapshot);
                }
                AgentResponse::Exit => {
                    should_quit_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ProviderSwitched { provider, model } => {
                    let mut msgs = messages_clone.write().await;
                    push_local_notice(
                        &mut msgs,
                        NoticeSeverity::Info,
                        format!("Provider switched to {} ({})", provider, model),
                    );
                    *cp_clone.lock().await = provider;
                    *cm_clone.lock().await = model;
                }
                AgentResponse::Error(msg) => {
                    let mut msgs = messages_clone.write().await;
                    push_local_notice(&mut msgs, NoticeSeverity::Error, msg);
                }
            }
        }
    });

    let messages_for_loop = messages.clone();

    let mut app = App {
        input: String::new(),
        messages: Vec::new(),
        messages_version: 0,
        side_messages: Vec::new(),
        side_messages_version: 0,
        layout_height_cache: Default::default(),
        in_side_view: false,
        side_session_id: None,
        parent_status: ParentStatus::Idle,
        scroll: 0,
        follow_bottom: true,
        content_lines: 0,
        view_height: 0,
        max_scroll: 0,
        sticky_step: None,
        sticky_rect: None,
        activity_rect: None,
        todos_rect: None,
        modal_rect: None,
        sticky_summary_line: None,
        pin_summary_line: None,
        focus_stack: Vec::new(),
        tx,
        should_quit,
        serve_tap: serve_tap_for_app,
        serve_cancel: None,
        suggestion_index: None,
        completion_dismissed: false,
        custom_commands,
        cursor_position: 0,
        input_scroll: 0,
        active_modal: Modal::None,
        modal_index: 0,
        session_scroll: 0,
        session_modal_follow: true,
        permissions_scroll: 0,
        history_scroll: 0,
        history_modal_follow: true,
        history_preview: false,
        history_search: false,
        current_provider: initial_provider,
        current_model: initial_model,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        path_scan_cache: None,
        current_pursuit: None,
        session_context: None,
        loop_status: "idle".to_string(),
        activity_status: String::new(),
        unattended: false,
        todos: None,
        turn_count: 0,
        current_round: 0,
        review_alert: String::new(),
        turn_started_at: None,
        activity_tab: ActivityTab::Activity,
        activity_scroll: 0,
        help_scroll: 0,
        pending_permission: None,
        question: None,
        question_scroll: 0,
        question_modal_follow: true,
        sessions_overview: Vec::new(),
        permission_confirm_always: false,
        permission_show_details: false,
        permission_scroll: 0,
        permission_max_scroll: 0,
        input_history,
        history_index: None,
        history_draft: String::new(),
        pending_images: Vec::new(),
        pending_text_pastes: Vec::new(),
        pending_dispatch: std::collections::VecDeque::new(),
        selection: SelectionState::None,
        drag: SelectionDrag::default(),
        layout_map: LayoutMap::new(),
        modal_hit_map: crate::tui::layout::ModalHitMap::new(),
        hovered_step: None,
        tool_density: tool_density.clone(),
        focused_target: None,
        cursor_hidden: false,
        copy_toast_until: None,
        copy_toast_message: String::new(),
        copy_toast_failed: false,
        ctrl_c_armed_ticks: 0,
        esc_armed_ticks: 0,
        spinner_epoch: std::time::Instant::now(),
        stashed_input: String::new(),
        editor_target: None,
        editor_field: 0,
        editor_key: String::new(),
        editor_model: String::new(),
        model_search: false,
        model_scroll: 0,
        model_modal_follow: true,
        key_status: HashMap::new(),
        provider_picker: ProviderPickerSnapshot::default(),
        theme: Theme::default(),
        logo: load_user_logo(),
        mcp_statuses,
    };

    // Run app
    let res = event_loop::run_app_loop(
        &mut terminal,
        &mut app,
        event_loop::UiRuntime {
            current_provider,
            current_model,
            harness,
            activity_status,
            pending_permission,
            pending_question,
            is_responding,
            dirty,
            dirty_notify,
            subagent_permission_parent,
            subagent_question_parent,
            messages: messages_for_loop,
            side_messages,
            parent_status,
            side_view_signal,
            key_status,
            provider_picker,
            sessions_overview,
            open_sessions,
            session_context,
            todos,
            turn_count,
            current_round,
            review_alert,
            turn_started_at,
        },
        session,
    )
    .await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.writer(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        return Err(err.into());
    }

    Ok(app.input_history)
}

#[allow(clippy::too_many_arguments)]
pub async fn start_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
    initial_messages: Vec<Message>,
    custom_commands: Vec<(String, String)>,
    mcp_statuses: Vec<(String, McpConnectionStatus)>,
    tui_config: config::TuiConfig,
    session: Arc<SessionStore>,
) -> Result<Vec<String>, Box<dyn Error>> {
    run_tui(
        tx,
        rx,
        initial_provider,
        initial_model,
        input_history,
        initial_messages,
        custom_commands,
        mcp_statuses,
        tui_config,
        session,
    )
    .await
}

fn push_core_notice(messages: &mut Vec<TranscriptMessage>, notice: &neenee_core::AgentNotice) {
    let _surface = notice.surface;
    messages.push(TranscriptMessage::notice(
        notice_severity_from_core(notice.severity),
        notice.render_text(),
    ));
}

fn push_local_notice(
    messages: &mut Vec<TranscriptMessage>,
    severity: NoticeSeverity,
    text: impl Into<String>,
) {
    messages.push(TranscriptMessage::notice(severity, text));
}

/// Format an inline-transcript notice for a pursuit update, or `None` when the
/// update carries nothing user-visible (a no-op re-broadcast of the same
/// pursuit). The pursuit bar is gone from the footer; these notices are how pursuit
/// changes now scroll with the transcript instead of living in a pinned bar.
fn describe_pursuit_change(prev: Option<&Pursuit>, new: &Pursuit) -> Option<String> {
    let summary = |prefix: &str| -> String { format!("{prefix} · {}", new.objective) };
    if new.is_complete && prev.is_none_or(|p| !p.is_complete) {
        return Some(summary("✓ pursuit complete"));
    }
    let prev = prev?;
    if prev.objective != new.objective {
        return Some(summary("pursue"));
    }
    None
}

/// Format a single inline-transcript notice for a task-list update. Task-list
/// changes are the agent's own bookkeeping — full per-item detail lives in the
/// Activity modal — so the transcript never fans them out into one line per
/// changed step. Instead every update collapses to **at most one** summary line:
/// the running `done/total` tally, optionally annotated with how many items
/// changed status this turn. Returns `None` when nothing changed.
#[cfg(test)]
fn describe_todos_change(prev: Option<&TodoList>, new: Option<&TodoList>) -> Option<String> {
    let new = new.filter(|l| !l.items.is_empty())?;
    let done = new.count(neenee_core::TodoStatus::Completed);
    let total = new.items.len();
    let Some(prev) = prev.filter(|l| !l.items.is_empty()) else {
        return Some(format!("tasks started · {done}/{total}"));
    };
    // Count status transitions across the items present in both snapshots.
    // Newly added items (no positional counterpart) do not read as a status
    // *change* and are absorbed into the tally rather than flagged here.
    let changed = prev
        .items
        .iter()
        .zip(new.items.iter())
        .filter(|(a, b)| a.status != b.status)
        .count();
    if changed == 0 && prev.items.len() == new.items.len() {
        return None;
    }
    // One compact line: progress tally plus — only when something actually
    // moved — how many steps changed this turn.
    if changed > 0 {
        Some(format!("tasks · {done}/{total} · {changed} updated"))
    } else {
        Some(format!("tasks · {done}/{total}"))
    }
}

#[cfg(test)]
mod describe_todos_change_tests {
    //! Behaviour contract for the single-line task-list transcript notice.
    //! The point of condensing is that *every* update — even one that ticks
    //! five steps at once — yields at most one `ℹ` line, not a fan-out.
    use super::*;
    use neenee_core::{TodoId, TodoItem, TodoStatus};

    fn item(id: u64, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: TodoId(id),
            content: format!("step {id}"),
            status,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn list(items: &[TodoItem]) -> TodoList {
        TodoList {
            items: items.to_vec(),
            ..TodoList::default()
        }
    }

    #[test]
    fn first_appearance_announces_started_with_tally() {
        let new = list(&[
            item(1, TodoStatus::InProgress),
            item(2, TodoStatus::Pending),
            item(3, TodoStatus::Pending),
        ]);
        // No previous list → the "started" line, counting completed (0/3).
        assert_eq!(
            describe_todos_change(None, Some(&new)),
            Some("tasks started · 0/3".to_string())
        );
    }

    #[test]
    fn multiple_status_changes_collapse_to_one_line() {
        // The regression this guards: previously each changed step emitted its
        // own `ℹ` line. Now five simultaneous ticks produce exactly one.
        let prev = list(&[
            item(1, TodoStatus::Pending),
            item(2, TodoStatus::Pending),
            item(3, TodoStatus::InProgress),
            item(4, TodoStatus::Pending),
            item(5, TodoStatus::Pending),
        ]);
        let new = list(&[
            item(1, TodoStatus::Completed),
            item(2, TodoStatus::Completed),
            item(3, TodoStatus::Completed),
            item(4, TodoStatus::InProgress),
            item(5, TodoStatus::Cancelled),
        ]);
        assert_eq!(
            describe_todos_change(Some(&prev), Some(&new)),
            Some("tasks · 3/5 · 5 updated".to_string())
        );
    }

    #[test]
    fn single_status_change_counts_one() {
        let prev = list(&[
            item(1, TodoStatus::Pending),
            item(2, TodoStatus::InProgress),
        ]);
        let new = list(&[item(1, TodoStatus::Pending), item(2, TodoStatus::Completed)]);
        assert_eq!(
            describe_todos_change(Some(&prev), Some(&new)),
            Some("tasks · 1/2 · 1 updated".to_string())
        );
    }

    #[test]
    fn no_change_emits_nothing() {
        let same = list(&[
            item(1, TodoStatus::InProgress),
            item(2, TodoStatus::Pending),
        ]);
        assert_eq!(describe_todos_change(Some(&same), Some(&same)), None);
    }

    #[test]
    fn size_only_change_drops_the_updated_suffix() {
        // Items added without any positional status change: still one line,
        // but without the "N updated" suffix since nothing transitioned.
        let prev = list(&[item(1, TodoStatus::Pending)]);
        let new = list(&[
            item(1, TodoStatus::Pending),
            item(2, TodoStatus::Pending),
            item(3, TodoStatus::Pending),
        ]);
        assert_eq!(
            describe_todos_change(Some(&prev), Some(&new)),
            Some("tasks · 0/3".to_string())
        );
    }

    #[test]
    fn empty_new_list_emits_nothing() {
        let prev = list(&[item(1, TodoStatus::Pending)]);
        assert_eq!(
            describe_todos_change(Some(&prev), Some(&TodoList::default())),
            None
        );
        assert_eq!(
            describe_todos_change(None, Some(&TodoList::default())),
            None
        );
    }
}

/// Load the user-supplied ASCII logo from `$XDG_CONFIG_HOME/neenee/logo.txt`,
/// clamped to the empty-state bounding box. Best-effort: a missing or unreadable
/// file returns `None`, leaving the built-in wordmark in place.
fn load_user_logo() -> Option<Vec<String>> {
    let path = neenee_store::paths::get().logo_file();
    let raw = std::fs::read_to_string(&path).ok()?;
    // Re-use the renderer's parser so the clamp stays defined in one place.
    // The parser already strips CRLF/trailing blanks and truncates to the box.
    render::parse_logo(&raw)
}

#[cfg(test)]
mod tests;
