pub mod app;
pub mod clipboard;
pub mod clipboard_ops;
pub mod completion;
pub mod config;
pub mod document;
mod event_loop;
pub mod fuzzy;
pub mod input;
pub mod layout;
pub mod models;
pub mod render;
pub mod selection;
pub mod step_interaction;
mod terminal;
mod transcript;

pub(crate) use app::{App, Modal, SessionTab};
pub(crate) use completion::{Completion, CompletionKind};
pub(crate) use models::{
    model_context_window, model_display_name, models_filtered_from, ModelSolution, SOLUTIONS,
};

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use neenee_core::{
    mcp::McpConnectionStatus, AgentMode, AgentRequest, AgentResponse, HarnessSnapshot, Message,
    ModelPickerSnapshot, PermissionRequest, PlanProgress, Role, SessionContextSnapshot,
    SessionOverview, UserQuestionRequest,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    io,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};

use crate::tui::document::{MessageKind, TranscriptMessage};
use crate::tui::layout::LayoutMap;
use crate::tui::render::Theme;
use crate::tui::selection::{SelectionDrag, SelectionState};
use crate::tui::transcript::{finalize_streaming_reasoning, transcript_messages_from_core};

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
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;
    // Install the signal guard after the terminal enters raw mode + alt screen
    // so any later SIGTERM/SIGINT/SIGHUP restores it instead of stranding it.
    terminal::spawn_signal_guard();
    let tui_config = Arc::new(tui_config);
    let restored = transcript_messages_from_core(initial_messages, &tui_config);
    let messages = Arc::new(Mutex::new(restored));
    let messages_clone = messages.clone();
    let should_quit = Arc::new(AtomicBool::new(false));
    let should_quit_clone = should_quit.clone();

    let current_provider = Arc::new(Mutex::new(initial_provider.clone()));
    let current_model = Arc::new(Mutex::new(initial_model.clone()));
    let cp_clone = current_provider.clone();
    let cm_clone = current_model.clone();

    let is_responding = Arc::new(AtomicBool::new(false));
    let ir_clone = is_responding.clone();
    let harness = Arc::new(Mutex::new(HarnessSnapshot {
        mode: AgentMode::Build,
        goal: None,
        loop_status: "idle".to_string(),
        auto_approve: false,
    }));
    let harness_clone = harness.clone();
    let plan_progress: Arc<Mutex<Option<PlanProgress>>> = Arc::new(Mutex::new(None));
    let plan_progress_clone = plan_progress.clone();
    let activity_status = Arc::new(Mutex::new(String::new()));
    let activity_clone = activity_status.clone();
    let pending_permission = Arc::new(Mutex::new(VecDeque::<PermissionRequest>::new()));
    let pending_permission_clone = pending_permission.clone();
    let pending_question = Arc::new(Mutex::new(VecDeque::<UserQuestionRequest>::new()));
    let pending_question_clone = pending_question.clone();
    let key_status = Arc::new(Mutex::new(HashMap::<String, bool>::new()));
    let key_status_clone = key_status.clone();
    let model_picker = Arc::new(Mutex::new(ModelPickerSnapshot::default()));
    let model_picker_clone = model_picker.clone();
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

    // Spawn response listener
    tokio::spawn(async move {
        let mut reasoning_start: Option<std::time::Instant> = None;
        while let Some(resp) = rx.recv().await {
            match resp {
                AgentResponse::Text(t) => {
                    let (provider, model) = event_loop::attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(
                        TranscriptMessage::new(Role::Assistant, t)
                            .with_attribution(provider, model),
                    );
                    ir_clone.store(false, Ordering::SeqCst);
                    activity_clone.lock().await.clear();
                }
                AgentResponse::Activity(status) => {
                    *activity_clone.lock().await = status;
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::StreamStart => {
                    let (provider, model) = event_loop::attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(
                        TranscriptMessage::new(Role::Assistant, "")
                            .with_attribution(provider, model),
                    );
                    ir_clone.store(true, Ordering::SeqCst);
                    *activity_clone.lock().await = "responding".to_string();
                }
                AgentResponse::StreamDelta(delta) => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut() {
                        last.push_stream(&delta);
                    }
                }
                AgentResponse::StreamEnd(final_content) => {
                    ir_clone.store(true, Ordering::SeqCst);
                    *activity_clone.lock().await = "finalizing response".to_string();
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut() {
                        last.raw = final_content;
                        last.reparse();
                    }
                }
                AgentResponse::StreamDiscard => {
                    let mut msgs = messages_clone.lock().await;
                    if msgs
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant)
                    {
                        msgs.pop();
                    }
                }
                AgentResponse::StreamReasoningDelta(delta) => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut().filter(|message| message.is_thinking()) {
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
                        let (provider, model) = event_loop::attribution(&cp_clone, &cm_clone).await;
                        let mut thinking =
                            TranscriptMessage::thinking(delta).with_attribution(provider, model);
                        // A reasoning trace expands while streaming — watching
                        // the model think live is the value of the trace. On
                        // completion the transition leaves it as-is (no
                        // auto-collapse), so the user keeps what they were
                        // reading.
                        thinking.set_thinking_expanded(
                            step_interaction::default_thinking_expanded(true),
                        );
                        msgs.push(thinking);
                        reasoning_start = Some(std::time::Instant::now());
                    }
                }
                AgentResponse::StreamReasoningEnd(content) => {
                    let duration_ms = reasoning_start
                        .take()
                        .map(|started| started.elapsed().as_millis() as u64);
                    let mut msgs = messages_clone.lock().await;
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
                AgentResponse::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    *activity_clone.lock().await =
                        event_loop::tool_activity_status(&name).to_string();
                    let (provider, model) = event_loop::attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    // A tool step starts collapsed: there's no result to show
                    // yet. The lifecycle-aware default (see `step_interaction`)
                    // expands it on completion — Ok follows per-tool density,
                    // Failed/Denied force-expand to surface the error.
                    let message = TranscriptMessage::tool_step(id, name, arguments)
                        .with_attribution(provider, model);
                    msgs.push(message);
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ToolResult {
                    id,
                    name,
                    output,
                    structured,
                    duration_ms,
                } => {
                    *activity_clone.lock().await = "thinking".to_string();
                    let (provider, model) = event_loop::attribution(&cp_clone, &cm_clone).await;
                    let density = tool_density_clone.load(Ordering::SeqCst);
                    let mut msgs = messages_clone.lock().await;
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
                AgentResponse::ToolCancelled { id, .. } => {
                    // Convergence: an in-flight call was aborted by an
                    // interrupt. Flip its step (and any nested sub-agent
                    // children) to Cancelled so it never stays "running".
                    let mut msgs = messages_clone.lock().await;
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
                        let mut message = TranscriptMessage::tool_step(id.clone(), "tool", "{}");
                        message.cancel_tool_step(&id);
                        message.set_tool_step_expanded(false);
                        msgs.push(message);
                    }
                }
                AgentResponse::ToolStream { id, stream } => {
                    // Live partial output from a running tool (e.g. bash
                    // stdout). Accumulate into the running step so it updates
                    // in place instead of freezing on a spinner.
                    let mut msgs = messages_clone.lock().await;
                    if !msgs
                        .iter_mut()
                        .any(|message| message.push_tool_stream(&id, &stream))
                    {
                        // Unknown id: drop silently — the matching ToolCall may
                        // have been dropped with an aborted turn.
                    }
                }
                AgentResponse::SubTask {
                    parent_call_id,
                    event,
                } => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(message) = msgs
                        .iter_mut()
                        .find(|m| m.is_tool_step() && matches!(&m.kind, crate::tui::document::MessageKind::ToolStep { id, .. } if id == &parent_call_id))
                    {
                        message.push_subtask_event(&event);
                    }
                }
                AgentResponse::PermissionRequest(request) => {
                    // A single model response can carry several write tool
                    // calls, each emitting its own request before blocking on
                    // its reply. Queue them FIFO so none is lost; the UI shows
                    // one sheet at a time and hands off as each is resolved.
                    pending_permission_clone.lock().await.push_back(request);
                    *activity_clone.lock().await = "awaiting permission".to_string();
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::PermissionsCleared => {
                    pending_permission_clone.lock().await.clear();
                    activity_clone.lock().await.clear();
                }
                AgentResponse::UserQuestionRequest(request) => {
                    pending_question_clone.lock().await.push_back(request);
                    *activity_clone.lock().await = "awaiting user input".to_string();
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ProviderKeys(status) => {
                    *key_status_clone.lock().await = status.into_iter().collect();
                }
                AgentResponse::ModelPicker(snapshot) => {
                    *model_picker_clone.lock().await = snapshot;
                }
                AgentResponse::ConversationCleared => {
                    messages_clone.lock().await.clear();
                }
                AgentResponse::ConversationReplaced(messages) => {
                    *messages_clone.lock().await =
                        transcript_messages_from_core(messages, &tui_config_clone);
                }
                AgentResponse::SessionsOverview(sessions) => {
                    *sessions_overview_clone.lock().await = sessions;
                    open_sessions_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::SessionContext(snapshot) => {
                    *session_context_clone.lock().await = Some(snapshot);
                }
                AgentResponse::Compacted {
                    archived_messages,
                    before_chars,
                    after_chars,
                } => {
                    messages_clone.lock().await.push(TranscriptMessage::new(
                        Role::System,
                        format!(
                            "Compacted {} messages: {} -> {} chars.",
                            archived_messages, before_chars, after_chars
                        ),
                    ));
                }
                AgentResponse::HarnessState(snapshot) => {
                    let running = snapshot.loop_status != "idle";
                    *harness_clone.lock().await = snapshot;
                    ir_clone.store(running, Ordering::SeqCst);
                    if !running {
                        activity_clone.lock().await.clear();
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
                    let mut msgs = messages_clone.lock().await;
                    finalize_streaming_reasoning(&mut msgs, duration_ms);
                }
                AgentResponse::GoalUpdated(goal) => {
                    harness_clone.lock().await.goal = Some(goal);
                }
                AgentResponse::ModeChanged(mode) => {
                    harness_clone.lock().await.mode = mode;
                }
                AgentResponse::PlanProgressUpdated(progress) => {
                    *plan_progress_clone.lock().await = progress;
                }
                AgentResponse::AutoApproveChanged(enabled) => {
                    harness_clone.lock().await.auto_approve = enabled;
                }
                AgentResponse::RetryScheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    message,
                } => {
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
                AgentResponse::Error(e) => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(TranscriptMessage::new(
                        Role::System,
                        format!("Error: {}", e),
                    ));
                    ir_clone.store(false, Ordering::SeqCst);
                    activity_clone.lock().await.clear();
                }
                AgentResponse::Exit => {
                    should_quit_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ProviderSwitched { provider, model } => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(TranscriptMessage::new(
                        Role::System,
                        format!("System: Provider switched to {} ({})", provider, model),
                    ));
                    *cp_clone.lock().await = provider;
                    *cm_clone.lock().await = model;
                }
            }
        }
    });

    let messages_for_loop = messages.clone();

    let mut app = App {
        input: String::new(),
        messages: Vec::new(),
        scroll: 0,
        follow_bottom: true,
        content_lines: 0,
        view_height: 0,
        max_scroll: 0,
        sticky_step: None,
        sticky_rect: None,
        hint_goal_rect: None,
        sticky_summary_line: None,
        pin_summary_line: None,
        focus_stack: Vec::new(),
        tx,
        should_quit,
        suggestion_index: None,
        completion_dismissed: false,
        custom_commands,
        cursor_position: 0,
        input_scroll: 0,
        active_modal: Modal::None,
        modal_index: 0,
        session_tab: SessionTab::Model,
        session_scroll: 0,
        current_provider: initial_provider,
        current_model: initial_model,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        path_scan_cache: None,
        current_goal: None,
        session_context: None,
        loop_status: "idle".to_string(),
        activity_status: String::new(),
        auto_approve: false,
        plan_progress: None,
        pending_permission: None,
        pending_question: None,
        question_selected: Vec::new(),
        question_other_text: Vec::new(),
        question_current: 0,
        sessions_overview: Vec::new(),
        permission_confirm_always: false,
        permission_show_details: false,
        permission_scroll: 0,
        permission_max_scroll: 0,
        input_history,
        history_index: None,
        pending_images: Vec::new(),
        pending_dispatch: std::collections::VecDeque::new(),
        selection: SelectionState::None,
        drag: SelectionDrag::default(),
        layout_map: LayoutMap::new(),
        hovered_step: None,
        tool_density: tool_density.clone(),
        tui_config: tui_config.clone(),
        tool_detail_message_idx: None,
        tool_detail_scroll: 0,
        focused_target: None,
        focus_zone: input::FocusZone::Compose,
        cursor_hidden: false,
        copy_toast_until: None,
        copy_toast_message: String::new(),
        copy_toast_failed: false,
        ctrl_c_armed_ticks: 0,
        esc_armed_ticks: 0,
        spinner_tick: 0,
        stashed_input: String::new(),
        editor_target: None,
        editor_field: 0,
        editor_key: String::new(),
        editor_model: String::new(),
        key_status: HashMap::new(),
        model_picker: ModelPickerSnapshot::default(),
        theme: Theme::default(),
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
            messages: messages_for_loop,
            key_status,
            model_picker,
            sessions_overview,
            open_sessions,
            session_context,
            plan_progress,
        },
    )
    .await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
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
    )
    .await
}

#[cfg(test)]
mod tests;
