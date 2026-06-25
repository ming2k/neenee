//! The main TUI event/render loop and the helpers that only it needs.
//!
//! [`run_app_loop`] owns the per-frame work: sync shared runtime state into
//! [`App`], draw the chrome via the `render` modules, drain pending input
//! events through [`input::process_event`], and dispatch each
//! [`input::InputAction`] to its handler. State mutations almost always land
//! back on `App`; the few standalone helpers here cover status-text
//! formatting, message-tree navigation, and selection → clipboard extraction.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crossterm::event;
use ratatui::{backend::Backend, Terminal};
use tokio::sync::mpsc;

use neenee_core::{
    AgentRequest, HarnessSnapshot, ParentStatus, PermissionDecision, PermissionRequest,
    ProviderPickerSnapshot, Role, SessionOverview, TodoList, UserQuestionRequest,
};

use crate::tui::clipboard;
use crate::tui::clipboard_ops;
use crate::tui::completion::CompletionKind;
use crate::tui::composer_attachments;
use crate::tui::document::TranscriptMessage;
use crate::tui::input::{self};
use crate::tui::layout::{InteractiveTarget, InteractiveTargetKind, LayoutMap};
use crate::tui::render;
use crate::tui::selection::{
    floor_char_boundary, get_selected_text, inclusive_end, SelectionState,
};
use crate::tui::step_interaction;
use crate::tui::{ActivityTab, App, Modal, Recess, SessionTab, PROVIDERS};

use tokio::sync::Mutex;

/// Shared runtime state crossing the response-listener / event-loop boundary.
/// Each field is the single source of truth for one piece of live harness
/// state; the listener writes, the loop reads (after acquiring the per-field
/// mutex for one frame'snapshot).
pub(super) struct UiRuntime {
    pub current_provider: Arc<Mutex<String>>,
    pub current_model: Arc<Mutex<String>>,
    pub harness: Arc<Mutex<HarnessSnapshot>>,
    pub activity_status: Arc<Mutex<String>>,
    pub pending_permission: Arc<Mutex<VecDeque<PermissionRequest>>>,
    pub pending_question: Arc<Mutex<VecDeque<UserQuestionRequest>>>,
    pub is_responding: Arc<AtomicBool>,
    /// Full-duplex (ADR-0029): request_id → the parent tool-call id of the
    /// subagent that surfaced a permission or `ask_user` request (carried up
    /// as a `TurnEvent::SubAgent`). When the user answers in the modal, the
    /// loop looks the id up here to tag the reply with `parent_call_id` so the
    /// harness routes it down into the live child via the subagent registry.
    /// Top-level requests are absent here → `None` → legacy path. Kept as a
    /// side-table so the modal queue and rendering stay unchanged.
    pub subagent_permission_parent: Arc<Mutex<HashMap<String, String>>>,
    /// Companion to [`Self::subagent_permission_parent`] for `ask_user` replies.
    pub subagent_question_parent: Arc<Mutex<HashMap<String, String>>>,
    pub messages: Arc<Mutex<Vec<TranscriptMessage>>>,
    /// Side-conversation transcript buffer (ADR-0017). The listener appends
    /// per-turn events tagged with the side `session_id` here; the loop
    /// clones it into [`App::side_messages`] each frame while the side view
    /// is active.
    pub side_messages: Arc<Mutex<Vec<TranscriptMessage>>>,
    /// Coarse primary-session status, written by the listener from
    /// [`AgentResponse::ParentStatus`] and read into [`App::parent_status`]
    /// for the side banner (ADR-0017).
    pub parent_status: Arc<Mutex<ParentStatus>>,
    /// One-shot side-view transition (ADR-0017): `Opened` when the harness
    /// emits [`AgentResponse::SideViewOpened`] (the loop calls
    /// [`App::enter_side_view`]), `Closed` on [`AgentResponse::SideViewClosed`]
    /// ([`App::exit_side_view`]). Drained each frame.
    pub side_view_signal: Arc<Mutex<Option<SideViewSignal>>>,
    pub key_status: Arc<Mutex<HashMap<String, bool>>>,
    /// Model-picker snapshot shared with the response listener.
    pub provider_picker: Arc<Mutex<ProviderPickerSnapshot>>,
    /// Sessions picker rows + a one-shot request to open the picker modal.
    pub sessions_overview: Arc<Mutex<Vec<SessionOverview>>>,
    pub open_sessions: Arc<AtomicBool>,
    /// Latest session-context snapshot for the session modal, or `None` before
    /// the first `QuerySessionContext` round-trip completes. The modal renders
    /// a lightweight placeholder while this is `None`.
    pub session_context: Arc<Mutex<Option<neenee_core::SessionContextSnapshot>>>,
    /// Unified task list, mirrored from `AgentResponse::TodosUpdated`. The
    /// render loop copies it into `App::todos` each frame so the Activity
    /// modal stays in sync with the agent's state.
    pub todos: Arc<Mutex<Option<TodoList>>>,
    /// Live harness turn counter, mirrored from the harness snapshot so the
    /// task panel can reference the current turn.
    /// event channel.
    pub turn_count: Arc<Mutex<u64>>,
    /// Current tool round within the active turn (1-indexed for display).
    /// Set from `AgentResponse::RoundStarted`; reset to 0 at the turn
    /// boundary so the pre-request phase does not show a stale round.
    pub current_round: Arc<Mutex<u64>>,
    /// Session-review alert (ADR-0016), or empty when inactive. Mirrored into
    /// `App::review_alert` each frame; while non-empty the activity bar appends
    /// a `⚠ <alert>` segment.
    pub review_alert: Arc<Mutex<String>>,
    /// Wall-clock instant the current turn started, or `None` between turns.
    /// Set by the response listener on a "running" `HarnessState` and cleared
    /// on idle; drives the muted `<elapsed>` segment in the activity bar.
    pub turn_started_at: Arc<Mutex<Option<std::time::Instant>>>,
}

/// A pending `/btw` side-view transition queued by the response listener and
/// drained by the event loop (ADR-0017). `Opened` carries the side routing
/// key the listener needs to direct subsequent per-turn events to the side
/// buffer.
pub(super) enum SideViewSignal {
    Opened { side_id: String },
    Closed,
}

pub(super) async fn run_app_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    runtime: UiRuntime,
) -> io::Result<()> {
    let mut _copy_toast_timer: u8 = 0;
    // Clipboard copies run in background tasks so a slow/hanging system
    // clipboard (arboard/wl-copy) can never freeze the event loop.
    let (copy_tx, mut copy_rx) =
        mpsc::unbounded_channel::<Result<clipboard::CopyOutcome, String>>();
    // Number of clipboard copies still in flight. While this is non-zero the
    // event loop uses a short poll interval so the "copied" toast appears
    // within ~16ms of completion instead of waiting up to the full idle tick.
    let copy_pending = Arc::new(AtomicUsize::new(0));

    // Clipboard paste reads (Ctrl+V) run in background tasks for the same
    // reason: arboard/wl-paste must never block the event loop.
    let (paste_tx, mut paste_rx) = mpsc::unbounded_channel::<clipboard::ClipboardRead>();

    loop {
        if app.should_quit.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Apply any completed background clipboard copies.
        while let Ok(result) = copy_rx.try_recv() {
            clipboard_ops::set_copy_feedback(app, result);
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(1800));
        }

        // Apply any completed clipboard paste reads.
        while let Ok(read) = paste_rx.try_recv() {
            clipboard_ops::apply_clipboard_paste(app, read);
        }

        // Sync provider/model from listener
        {
            app.current_provider = runtime.current_provider.lock().await.clone();
            app.current_model = runtime.current_model.lock().await.clone();
            let harness = runtime.harness.lock().await.clone();
            app.current_pursuit = harness.pursuit;
            app.loop_status = harness.loop_status;
            app.auto_approve = harness.auto_approve;
            app.activity_status = runtime.activity_status.lock().await.clone();
            app.session_context = runtime.session_context.lock().await.clone();
            app.todos = runtime.todos.lock().await.clone();
            app.turn_count = *runtime.turn_count.lock().await;
            app.current_round = *runtime.current_round.lock().await;
            app.review_alert = runtime.review_alert.lock().await.clone();
            app.turn_started_at = *runtime.turn_started_at.lock().await;
            app.pending_permission = runtime.pending_permission.lock().await.front().cloned();
            app.pending_question = runtime.pending_question.lock().await.front().cloned();
            app.key_status = runtime.key_status.lock().await.clone();
            app.provider_picker = runtime.provider_picker.lock().await.clone();
            if app.pending_permission.is_some() && app.active_modal == Modal::None {
                app.active_modal = Modal::Permission;
                app.modal_index = 0;
                app.permission_scroll = 0;
                app.permission_show_details = false;
                // A permission prompt is urgent: hand keyboard focus to the
                // sheet so the next keypress decides it, not the transcript.
                app.focus_zone = input::FocusZone::Compose;
            } else if app.pending_permission.is_none() && app.active_modal == Modal::Permission {
                app.active_modal = Modal::None;
                app.modal_index = 0;
                app.permission_confirm_always = false;
                app.permission_scroll = 0;
                app.permission_max_scroll = 0;
                app.permission_show_details = false;
            }
            if app.pending_question.is_some() && app.active_modal == Modal::None {
                app.active_modal = Modal::Question;
                app.modal_index = 0;
                app.question_current = 0;
                // Default selection: empty for multi-select, first option for single-select.
                if let Some(ref request) = app.pending_question {
                    app.question_selected = request
                        .questions
                        .iter()
                        .map(|q| if q.multi_select { Vec::new() } else { vec![0] })
                        .collect();
                    app.question_other_text =
                        request.questions.iter().map(|_| String::new()).collect();
                }
                app.focus_zone = input::FocusZone::Compose;
            } else if app.pending_question.is_none() && app.active_modal == Modal::Question {
                app.active_modal = Modal::None;
                app.modal_index = 0;
                app.question_current = 0;
                app.question_selected.clear();
                app.question_other_text.clear();
            }
            // Sessions picker: refresh rows and open the modal on request.
            app.sessions_overview = runtime.sessions_overview.lock().await.clone();
            if runtime.open_sessions.swap(false, Ordering::SeqCst)
                && app.active_modal != Modal::Permission
            {
                app.active_modal = Modal::Sessions;
                app.modal_index = 0;
            }
        }

        // Decrement toast timers
        if let Some(until) = app.copy_toast_until {
            if std::time::Instant::now() >= until {
                app.copy_toast_until = None;
            }
        }
        // While images are staged for the next message, keep a persistent
        // indicator visible so the user knows Enter will send them. Skipped
        // while the Ctrl+C quit window is armed so a freshly-shown
        // "input cleared — Ctrl+C again to exit" toast keeps the floor and
        // is not immediately overwritten by the per-frame image reminder.
        if !app.pending_images.is_empty() && app.ctrl_c_armed_ticks == 0 {
            let n = app.pending_images.len();
            app.copy_toast_message = format!(
                "{n} image{} attached — enter to send",
                if n == 1 { "" } else { "s" }
            );
            app.copy_toast_failed = false;
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(600));
        }
        if app.ctrl_c_armed_ticks > 0 {
            app.ctrl_c_armed_ticks -= 1;
        }
        // The Esc armed toast only makes sense while a task is running; once
        // the turn finishes there is nothing left to interrupt, so let it
        // expire immediately rather than mislead the user.
        if app.esc_armed_ticks > 0 {
            if runtime.is_responding.load(Ordering::SeqCst) {
                app.esc_armed_ticks -= 1;
            } else {
                app.esc_armed_ticks = 0;
            }
        }

        // Pull messages from the shared lock into app state for rendering
        app.messages = runtime.messages.lock().await.clone();
        // Mirror the side buffer + parent status for the `/btw` banner
        // (ADR-0017). Cloned unconditionally: cheap relative to a frame, and
        // the side buffer may update while the view is open even if the user
        // briefly returns to the primary transcript.
        app.side_messages = runtime.side_messages.lock().await.clone();
        app.parent_status = *runtime.parent_status.lock().await;
        // Drain a pending side-view transition (enter/leave `/btw`).
        match runtime.side_view_signal.lock().await.take() {
            Some(crate::tui::event_loop::SideViewSignal::Opened { side_id, .. }) => {
                app.enter_side_view(side_id);
            }
            Some(crate::tui::event_loop::SideViewSignal::Closed) => {
                app.exit_side_view();
            }
            None => {}
        }

        // Drain the send queue when the harness returns to idle. The
        // response listener flips `is_responding` to false on the
        // `loop_status == "idle"` HarnessState snapshot, so reaching here
        // with a non-empty `pending_dispatch` means a turn just finished
        // (or the app just started) and the next queued user message is
        // ready to ship. FIFO: the front of the queue pairs with the first
        // transcript message still carrying `DeliveryStatus::Queued`.
        if !runtime.is_responding.load(Ordering::SeqCst)
            && app.loop_status == "idle"
            && !app.pending_dispatch.is_empty()
        {
            let dispatch = app
                .pending_dispatch
                .pop_front()
                .expect("checked non-empty above");
            let mut messages = runtime.messages.lock().await;
            let flipped = messages
                .iter_mut()
                .find(|m| {
                    m.role == Role::User
                        && m.delivery == crate::tui::document::DeliveryStatus::Queued
                })
                .map(|m| {
                    m.delivery = crate::tui::document::DeliveryStatus::Delivered;
                })
                .is_some();
            drop(messages);
            // Defensive: if the transcript lost its marker (shouldn't happen
            // in normal flow), we still want to ship the user's message —
            // the queue is the source of truth for dispatch.
            let _ = flipped;
            runtime.is_responding.store(true, Ordering::SeqCst);
            *runtime.activity_status.lock().await = "queued".to_string();
            // Expand paste chips at dispatch time so the model receives the
            // real paste contents. Image chips stay as positional labels in
            // the text; their payloads ship via `images`.
            let expanded_text =
                composer_attachments::expand_paste_chips(&dispatch.text, &dispatch.text_pastes);
            let _ = app.tx.send(AgentRequest::Chat {
                text: expanded_text,
                images: dispatch.images,
            });
        }

        // While following, keep the newest content in view using the previous
        // frame's measurement (max_scroll is recomputed after each draw).
        if app.follow_bottom {
            app.scroll = app.max_scroll;
        }

        // Advance the status-bar spinner phase for this frame. The draw call
        // only reads it, so a single wrapping increment per frame gives a
        // smooth ~10 fps braille animation tied to the 100ms event poll.
        app.spinner_tick = app.spinner_tick.wrapping_add(1);

        // Draw frame
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            let status = display_status(
                &app.loop_status,
                &app.activity_status,
                app.pending_permission.is_some(),
            );

            // Compute the displayed input text first so the transcript layout can
            // reserve the right height for a wrapping, growing input box.
            let masked_input = if app.active_modal == Modal::ModelEditor && app.editor_field == 0 {
                // Mask the API key everywhere it could be rendered (the editor
                // field itself, and any layout pass that inspects the input).
                "•".repeat(app.input.chars().count())
            } else {
                app.input.clone()
            };

            // Modal recess policy (single source of truth: `Modal::recess`).
            // A terminal cannot alpha-blend, so a modal either floats, darkens
            // the live surface in place, or fully occludes it:
            // - Takeover (Sessions): the footer collapses to zero height and
            //   the surface is occluded — opening a different session is a full
            //   context switch, so a clean slate is the intent.
            // - Dim (every other centered modal): the footer keeps its height
            //   so layout is stable, and the whole surface is darkened in place
            //   by the recess pass just before the modal is drawn. Context
            //   (transcript, input, hint bar, activity bar) stays visible for
            //   focus while the centered panel reads as the focal layer.
            // - None (Question / Permission): floats on the fully-live surface.
            // Provider / ModelEditor / HistorySearch borrow the input line as
            // their own field, so the composer is suppressed for them (its rect
            // stays as recessed surface) — no duplicate field, and no
            // masked-cursor panic in the editor.
            let recess = app.active_modal.recess();
            let chrome_hidden = recess == Recess::Takeover;

            // When zoomed into a subagent, render its child messages and show
            // a navigation bar; otherwise render the root conversation.
            let view_messages = app.focused_messages();
            // `/btw` side banner (ADR-0017): shown only while the side view is
            // active. The subagent zoom and the side view are mutually
            // exclusive, so the two banners never coexist.
            let side_banner = app.in_side_view.then_some(app.parent_status);
            let subagent_bar = app.focus_stack.last().and_then(|current| {
                let tasks: Vec<&TranscriptMessage> = app
                    .messages
                    .iter()
                    .filter(|message| message.is_subagent_task())
                    .collect();
                let idx = tasks
                    .iter()
                    .position(|message| message.tool_step_call_id() == Some(current.as_str()))?;
                Some(render::SubagentBarInfo {
                    label: tasks.get(idx)?.subagent_label(),
                    index: idx + 1,
                    total: tasks.len(),
                })
            });

            // Suppress the hover affordance whenever a full-overlay modal is
            // open so no stale highlight bleeds through. The permission sheet
            // keeps the transcript interactive, so it is exempted.
            let chrome_interactive = matches!(app.active_modal, Modal::None | Modal::Permission);

            let transcript_render = render::draw_transcript(
                f,
                &mut layout_map,
                render::TranscriptView {
                    messages: view_messages,
                    scroll: app.scroll,
                    selection: &app.selection,
                    activity: &status,
                    spinner_phase: app.spinner_tick,
                    input: &masked_input,
                    byte_cursor: app.byte_cursor(),
                    chrome_hidden,
                    subagent_bar,
                    side_banner,
                    pursuit: app.current_pursuit.as_ref(),
                    todos: app.todos.as_ref(),
                    review_alert: app.review_alert.clone(),
                    turn_started_at: app.turn_started_at,
                    hovered_step: chrome_interactive.then_some(app.hovered_step).flatten(),
                    focused_target: chrome_interactive.then_some(app.focused_target).flatten(),
                    theme: &app.theme,
                },
            );
            let input_rect = transcript_render.input_rect;
            let hint_rect = transcript_render.hint_rect;
            let activity_rect = transcript_render.activity_rect;
            let content_lines = transcript_render.content_lines;
            let view_height = transcript_render.view_height;
            let sticky = transcript_render.sticky;

            // The hint bar (model / context) lives directly below the input
            // box. Rendered only when the chrome is visible. It is drawn before
            // the composer because it borrows `view_messages` (an immutable
            // borrow of `app`) while `draw_composer` needs a mutable borrow of
            // `app.input_scroll`.
            // The permission sheet takes over the hint line as well as the
            // input box, so suppress the hint bar while it is open.
            if !chrome_hidden && hint_rect.height > 0 && app.active_modal != Modal::Permission {
                render::draw_hint_bar(
                    f,
                    hint_rect,
                    render::HintBarView {
                        current_provider: &app.current_provider,
                        current_model: &app.current_model,
                        messages: view_messages,
                        focus_zone: app.focus_zone,
                        shell_active: app.focus_zone.is_compose()
                            && app.active_modal == Modal::None
                            && app.input.starts_with('!'),
                        auto_approve: app.auto_approve,
                    },
                    &app.theme,
                );
            }

            // The input box is only shown when no overlay modal is open. The
            // `focused` flag drops the panel to its dim "blurred" palette and
            // hides the caret whenever keyboard focus is on the conversation
            // stream (Browse zone), so the user can see at a glance which
            // surface the next keypress will land on. A pending permission
            // request replaces the composer with the inline permission sheet.
            if !chrome_hidden {
                if app.active_modal == Modal::Permission {
                    if let Some(request) = app.pending_permission.as_ref() {
                        // Extend the slot down by the hint-line height so the
                        // sheet also covers (replaces) the hint bar.
                        let permission_rect = ratatui::layout::Rect::new(
                            input_rect.x,
                            input_rect.y,
                            input_rect.width,
                            input_rect.height + hint_rect.height,
                        );
                        let max_scroll = render::draw_permission_sheet(
                            f,
                            request,
                            app.modal_index,
                            app.permission_confirm_always,
                            app.permission_show_details,
                            app.permission_scroll,
                            permission_rect,
                            &app.theme,
                        );
                        app.permission_max_scroll = max_scroll;
                        app.permission_scroll =
                            app.permission_scroll.min(app.permission_max_scroll);
                    }
                } else if matches!(
                    app.active_modal,
                    Modal::Provider | Modal::ModelEditor | Modal::HistorySearch
                ) {
                    // These modals borrow the input line as their own field
                    // (filter / key+model / search), so the composer underneath
                    // would only duplicate the same `app.input` the modal
                    // already shows. Its rect stays mounted (so the footer
                    // layout is stable) but is left as recessed surface — the
                    // dim pass darkens it like the rest of the background. For
                    // the editor's key field the composer would also panic:
                    // the masked key's byte cursor is computed against the
                    // unmasked string.
                } else if !app.in_subagent_view() {
                    // The composer stays mounted for the dim-recess modals
                    // (Help / ToolStepDetail / Session /
                    // Activity) so the footer layout doesn't shift when the
                    // overlay opens or closes; the recess pass darkens it in
                    // place with the rest of the surface. The caret is hidden
                    // whenever any modal owns the keyboard.
                    let compose_focused = app.focus_zone.is_compose();
                    let show_caret = compose_focused && app.active_modal == Modal::None;
                    render::draw_composer(
                        f,
                        input_rect,
                        &app.input,
                        app.byte_cursor(),
                        compose_focused,
                        show_caret,
                        &app.theme,
                        &mut layout_map,
                        true,
                        &mut app.input_scroll,
                        &app.selection,
                    );
                }
            }

            // Now that `view_messages` is no longer borrowed, persist the
            // per-frame layout state back onto `app` for the next iteration
            // and for click routing.
            app.content_lines = content_lines;
            app.view_height = view_height;
            app.activity_rect = activity_rect;
            match sticky {
                Some(info) => {
                    app.sticky_step = Some(info.message_idx);
                    app.sticky_rect = Some(info.rect);
                    app.sticky_summary_line = Some(info.summary_line);
                }
                None => {
                    app.sticky_step = None;
                    app.sticky_rect = None;
                    app.sticky_summary_line = None;
                }
            }

            // Completion menu: slash commands or `@path` file mentions.
            // Honors `completion_dismissed` so Esc / Enter-commit keep the
            // popup hidden until the next edit clears the latch.
            if app.active_modal == Modal::None
                && !app.completion_dismissed
                && app.completion_kind() != CompletionKind::None
            {
                let completions = app.completions();
                if !completions.is_empty() {
                    render::draw_completion_menu(
                        f,
                        &mut layout_map,
                        &completions,
                        app.suggestion_index,
                        input_rect,
                        &app.theme,
                    );
                }
            }

            // Recess the live surface for the open modal: darken it in place
            // (Dim), occlude it fully (Takeover), or leave it untouched (None).
            // Done after the transcript + chrome are drawn and before the modal
            // panel so the panel overpaints its own crisp area on top of the
            // recessed background.
            render::recess_backdrop(f, recess, &app.theme);

            // Modals
            match app.active_modal {
                Modal::Provider => {
                    render::draw_models_modal(
                        f,
                        &mut layout_map,
                        PROVIDERS,
                        &app.current_provider,
                        app.modal_index,
                        &app.key_status,
                        &app.provider_picker,
                        &app.input,
                        app.cursor_position,
                        &app.theme,
                    );
                }
                Modal::HistorySearch => {
                    let ranked = app.history_filtered();
                    render::draw_history_modal(
                        f,
                        &mut layout_map,
                        &app.input_history,
                        &app.input,
                        app.cursor_position,
                        &ranked,
                        app.modal_index,
                        &app.theme,
                    );
                }
                Modal::Permission => {}
                Modal::Question => {
                    if let Some(ref request) = app.pending_question {
                        render::draw_question_modal(
                            f,
                            request,
                            app.question_current,
                            &app.question_selected,
                            &app.question_other_text,
                            app.modal_index,
                            &app.theme,
                        );
                    }
                }
                Modal::ModelEditor => {
                    let title = app
                        .editor_target
                        .and_then(|idx| PROVIDERS.get(idx))
                        .map(|s| s.name)
                        .unwrap_or("model");
                    render::draw_model_editor(
                        f,
                        title,
                        app.editor_field,
                        &app.editor_key,
                        &app.editor_model,
                        &app.input,
                        app.cursor_position,
                        &app.theme,
                    );
                }
                Modal::Help => render::draw_help_modal(f, &app.theme),
                Modal::ToolStepDetail => {
                    if let Some(msg) = app
                        .tool_detail_message_idx
                        .and_then(|idx| app.messages.get(idx))
                    {
                        render::draw_tool_step_detail_overlay(
                            f,
                            msg,
                            app.tool_detail_scroll,
                            &app.theme,
                        );
                    }
                }
                Modal::Sessions => render::draw_sessions_modal(
                    f,
                    &app.sessions_overview,
                    app.modal_index
                        .min(app.sessions_overview.len().saturating_sub(1)),
                    &app.theme,
                ),
                Modal::Session => render::draw_session_modal(
                    f,
                    app.session_tab,
                    &app.current_provider,
                    &app.current_model,
                    &app.key_status,
                    &app.mcp_statuses,
                    app.session_context.as_ref(),
                    app.modal_index,
                    &mut app.session_scroll,
                    &app.theme,
                ),
                Modal::Activity => {
                    let user_prompt: Option<String> = app
                        .focused_messages()
                        .iter()
                        .rev()
                        .find(|m| m.role == neenee_core::Role::User)
                        .map(|m| m.raw.clone());
                    render::draw_activity_modal(
                        f,
                        render::ActivityModalView {
                            active_tab: app.activity_tab,
                            pursuit: app.current_pursuit.as_ref(),
                            todos: app.todos.as_ref(),
                            user_prompt: user_prompt.as_deref(),
                            turn_count: app.turn_count,
                            current_round: app.current_round,
                            review_alert: &app.review_alert,
                            current_model: app.current_model.as_str(),
                            turn_started_at: app.turn_started_at,
                            activity: &status,
                        },
                        &mut app.activity_scroll,
                        &app.theme,
                    )
                }
                Modal::None => {}
            }

            // Copy toast
            if app.copy_toast_until.is_some() {
                render::draw_copy_toast(
                    f,
                    &app.copy_toast_message,
                    app.copy_toast_failed,
                    &app.theme,
                );
            } else if app.ctrl_c_armed_ticks > 0 {
                // The copy toast and the armed toast render at the same
                // screen position, so only one shows at a time. The
                // clearing-input path surfaces the armed state through the
                // copy toast itself ("input cleared — Ctrl+C again to
                // exit"); once it expires, the standalone armed toast
                // takes over for the remainder of the quit window.
                render::draw_armed_toast(f, "press Ctrl+C again to exit", &app.theme);
            }
            if app.esc_armed_ticks > 0 {
                render::draw_armed_toast(f, "press Esc again to interrupt", &app.theme);
            }

            app.layout_map = layout_map;

            // Record the open modal's panel rect (when one is dismissable) so a
            // click on the backdrop outside it can close it. Computed from the
            // frame here, the same geometry the modal just drew with.
            let modal_rect = render::modal_outer_rect(&app.active_modal, f);
            app.modal_rect = modal_rect;
        })?;

        // Cursor visibility follows the focus zone so the caret only shows up
        // where keys actually land. While a modal is open the modal itself
        // owns the caret (and may hide it for non-edit modals like Help); in
        // Browse zone the input box is blurred so the caret is hidden too.
        // Toggled only when the desired state changes to avoid spamming the
        // terminal with redundant escape codes every frame.
        let cursor_should_hide = app.active_modal == Modal::None && app.focus_zone.is_browse();
        if cursor_should_hide != app.cursor_hidden {
            if cursor_should_hide {
                let _ = terminal.hide_cursor();
            } else {
                let _ = terminal.show_cursor();
            }
            app.cursor_hidden = cursor_should_hide;
        }

        // Recompute the bottom scroll offset for the next frame and keep the
        // manual scroll position within bounds when not following.
        let natural_max = app.content_lines.saturating_sub(app.view_height as usize) as u16;
        // `app.max_scroll` stays at the natural bottom so scroll shortcuts
        // (ScrollBottom / wheel down) still land on the real last page.
        app.max_scroll = natural_max;
        if !app.follow_bottom {
            // A collapsed sticky header may leave too little content below it
            // for `natural_max` to reach the header line; while a pin is
            // active, allow scrolling up to that line so the header stays at
            // the top of the viewport instead of being dragged back down.
            let limit = app
                .pin_summary_line
                .map(|line| natural_max.max(line.min(u16::MAX as usize) as u16))
                .unwrap_or(natural_max);
            app.scroll = app.scroll.min(limit);
        }
        app.retain_visible_focused_target();

        // Drain all currently-ready input events before redrawing. The first
        // event blocks for the normal poll interval; any further events the
        // terminal has already queued are coalesced with non-blocking polls
        // so they share a single redraw. Without this, pasting text triggers
        // one full screen redraw per pasted character.
        //
        // While a clipboard copy is in flight, shorten the idle poll so the
        // "copied" toast shows within ~16ms of the copy finishing.
        let mut events_drained = false;
        'event_batch: loop {
            let timeout = if events_drained {
                std::time::Duration::ZERO
            } else if copy_pending.load(Ordering::SeqCst) > 0 {
                std::time::Duration::from_millis(16)
            } else {
                std::time::Duration::from_millis(100)
            };
            if !event::poll(timeout)? {
                break;
            }
            events_drained = true;
            let event = event::read()?;
            // The Ctrl+R history-search modal borrows the input line as its
            // fuzzy query, so a literal `/foo` query must NOT trigger the slash
            // completion popup (or `@path` mentions). Suppress completions
            // entirely while that modal is open. The same suppression applies
            // right after an Enter-driven commit: the user just finished a
            // completion, so the popup should stay hidden until the next edit.
            let suppress_completions =
                app.active_modal == Modal::HistorySearch || app.completion_dismissed;
            // Pre-compute completion data to avoid borrow conflicts with process_event.
            let completions = if suppress_completions {
                Vec::new()
            } else {
                app.completions()
            };
            let suggestion_count = completions.len();
            // The "exact match" auto-accept on Enter only makes sense for slash
            // commands: there, typing an unambiguous prefix and pressing Enter
            // should expand to the unique command rather than send `/go` as a
            // (rejected) command. Path mentions are accepted only via Tab so
            // plain Enter still ships the message as the user typed it.
            let has_exact_suggestion = completions.iter().any(|c| {
                c.replace_start == 0 && c.replace_end == app.input.len() && c.label == app.input
            });
            let completion_kind = if suppress_completions {
                crate::tui::CompletionKind::None
            } else {
                app.completion_kind()
            };
            let in_subagent_view = app.in_subagent_view();
            let action = input::process_event(
                event,
                &mut app.input,
                &mut app.cursor_position,
                input::InputContext {
                    active_modal: app.active_modal,
                    is_responding: runtime.is_responding.load(Ordering::SeqCst),
                    completion_kind,
                    suggestion_count,
                    has_exact_suggestion,
                    suggestion_index: app.suggestion_index,
                    permission_confirm_always: app.permission_confirm_always,
                    permission_show_details: app.permission_show_details,
                    in_subagent_view,
                    in_side_view: app.in_side_view,
                    has_focused_target: app.focused_target.is_some(),
                    focus_zone: app.focus_zone,
                    has_queued: !app.pending_dispatch.is_empty(),
                },
                &mut app.drag,
            );
            if !app.input.is_empty() {
                app.focused_target = None;
                // Non-empty input implies the user is composing; make the zone
                // match so key bindings resolve to the input box.
                app.focus_zone = input::FocusZone::Compose;
            }

            match action {
                input::InputAction::None => {}
                input::InputAction::Quit => return Ok(()),
                input::InputAction::SendChat(text) => {
                    // Note: history-search selection no longer flows through
                    // here — Enter in `Modal::HistorySearch` emits the dedicated
                    // `HistoryInsert` action so the chosen entry lands in the
                    // input box for editing instead of being sent immediately.
                    app.active_modal = Modal::None;
                    app.suggestion_index = None;
                    app.input_scroll = 0;

                    // Stage the chips' backing payloads so they ship with
                    // this message. The text is expanded into the real paste
                    // contents at the moment of dispatch — either inline
                    // (immediate send) or when the queue drains (queued
                    // send). For queue recall, the raw chip text and the
                    // staged vectors are restored verbatim so the user can
                    // keep editing the placeholder.
                    let images = std::mem::take(&mut app.pending_images);
                    let text_pastes = std::mem::take(&mut app.pending_text_pastes);
                    let has_images = !images.is_empty();

                    if !text.is_empty() || has_images {
                        if runtime.is_responding.load(Ordering::SeqCst) {
                            // A turn is already in flight: stage the message
                            // in the send queue instead of cancelling the
                            // running turn. The transcript gets a distinct
                            // Queued marker so the user sees their message is
                            // pending, and the per-frame idle check drains
                            // the queue (FIFO) as soon as the harness returns
                            // to idle. Esc remains the explicit interrupt
                            // path; /slash and !shell commands still dispatch
                            // immediately (per the queue-scope decision).
                            app.pending_dispatch
                                .push_back(crate::tui::app::QueuedDispatch {
                                    text: text.clone(),
                                    images: images.clone(),
                                    text_pastes: text_pastes.clone(),
                                });
                            runtime
                                .messages
                                .lock()
                                .await
                                .push(TranscriptMessage::new(Role::User, text.clone()).queued());
                            if !text.is_empty() && app.input_history.last() != Some(&text) {
                                app.input_history.push(text.clone());
                            }
                            app.history_index = None;
                            app.follow_bottom = true;
                            app.pin_summary_line = None;
                        } else {
                            // Expand `[Pasted text #N +M lines]` chips into
                            // their full staged text right before dispatch so
                            // the model receives the real paste contents
                            // rather than the chip label. Image chips stay
                            // in the text as positional labels.
                            let expanded =
                                composer_attachments::expand_paste_chips(&text, &text_pastes);
                            runtime.is_responding.store(true, Ordering::SeqCst);
                            *runtime.activity_status.lock().await = "queued".to_string();
                            runtime
                                .messages
                                .lock()
                                .await
                                .push(TranscriptMessage::new(Role::User, text.clone()));
                            if !text.is_empty() && app.input_history.last() != Some(&text) {
                                app.input_history.push(text.clone());
                            }
                            app.history_index = None;
                            app.follow_bottom = true;
                            app.pin_summary_line = None;
                            let _ = app.tx.send(AgentRequest::Chat {
                                text: expanded,
                                images,
                            });
                        }
                    } else if let Some((start, end)) = app.selection.normalized_range() {
                        // Enter on a selected step: navigate into a subagent
                        // task, otherwise toggle that step's expansion.
                        if start.message_idx == end.message_idx {
                            let mi = start.message_idx;
                            let mut messages = runtime.messages.lock().await;
                            // A subagent task navigates into its view instead
                            // of expanding.
                            let enter_id = resolve_focused_mut(&mut messages, &app.focus_stack, mi)
                                .and_then(|message| {
                                    if message.is_subagent_task() {
                                        message.tool_step_call_id().map(String::from)
                                    } else {
                                        None
                                    }
                                });
                            if let Some(id) = enter_id {
                                drop(messages);
                                app.enter_subagent(id);
                            } else {
                                let toggled = app.toggle_step_pinned(&mut messages, mi);
                                drop(messages);
                                if toggled {
                                    app.selection = SelectionState::None;
                                }
                            }
                        }
                    }
                }
                input::InputAction::SendSlash(cmd) => {
                    app.suggestion_index = None;
                    app.input_scroll = 0;
                    runtime.is_responding.store(true, Ordering::SeqCst);
                    *runtime.activity_status.lock().await = "queued".to_string();
                    app.follow_bottom = true;
                    app.pin_summary_line = None;
                    runtime
                        .messages
                        .lock()
                        .await
                        .push(TranscriptMessage::new(Role::User, cmd.clone()));
                    if app.input_history.last() != Some(&cmd) {
                        app.input_history.push(cmd.clone());
                    }
                    app.history_index = None;
                    let _ = app.tx.send(AgentRequest::SlashCommand(cmd));
                }
                input::InputAction::SendShell(command) => {
                    // `!<command>` runs directly through the bash tool. We
                    // surface the literal `!command` the user typed as the
                    // transcript entry (so history recall shows the bang) but
                    // ship only the stripped command to the harness.
                    app.active_modal = Modal::None;
                    app.suggestion_index = None;
                    app.input_scroll = 0;
                    runtime.is_responding.store(true, Ordering::SeqCst);
                    *runtime.activity_status.lock().await = "queued".to_string();
                    app.follow_bottom = true;
                    app.pin_summary_line = None;
                    let display = format!("!{}", command);
                    runtime
                        .messages
                        .lock()
                        .await
                        .push(TranscriptMessage::new(Role::User, display.clone()));
                    if app.input_history.last() != Some(&display) {
                        app.input_history.push(display);
                    }
                    app.history_index = None;
                    let _ = app.tx.send(AgentRequest::ShellCommand { command });
                }
                input::InputAction::ProviderPickerActivate => {
                    if app.active_modal == Modal::Provider {
                        // Always activate the highlighted row of the filtered
                        // list. The cursor starts on the default model when the
                        // picker opens (see `OpenProvider`), so the "open + Enter"
                        // fast path still re-activates the default — but arrow-
                        // key navigation is now respected even when the user
                        // never typed a filter. Previously an empty filter
                        // forced `default_id`, so navigating to another row and
                        // pressing Enter silently re-activated the default.
                        let filtered = app.providers_filtered();
                        let target_id = filtered
                            .get(app.modal_index)
                            .or_else(|| filtered.first())
                            .map(|(i, _)| PROVIDERS[*i].id.to_string());
                        if let Some(id) = target_id {
                            if let Some((sol_idx, _)) = filtered
                                .iter()
                                .find(|(i, _)| PROVIDERS[*i].id == id)
                                .copied()
                            {
                                let solution = PROVIDERS[sol_idx];
                                if app.key_status.get(solution.id).copied().unwrap_or(true) {
                                    let _ = app.tx.send(AgentRequest::SwitchProvider {
                                        provider_type: solution.id.to_string(),
                                        model: solution.model.to_string(),
                                        api_key: None,
                                        base_url: None,
                                    });
                                    // The input box was borrowed as the filter;
                                    // restore the stashed draft and close.
                                    app.input = std::mem::take(&mut app.stashed_input);
                                    app.cursor_position = app.input.chars().count();
                                    app.active_modal = Modal::None;
                                } else {
                                    // No key configured: open the unified
                                    // editor so the user can enter a key (and
                                    // optionally override the model id) before
                                    // activating. The picker filter in `input`
                                    // is discarded — it is a transient search;
                                    // the original draft stays in stashed_input.
                                    app.editor_target = Some(sol_idx);
                                    app.editor_field = 0;
                                    app.editor_key.clear();
                                    app.editor_model = initial_editor_model(
                                        &solution,
                                        &app.current_provider,
                                        &app.current_model,
                                    );
                                    app.input.clear();
                                    app.cursor_position = 0;
                                    app.active_modal = Modal::ModelEditor;
                                }
                            }
                        }
                    }
                }
                input::InputAction::ProviderPickerToggleFavorite => {
                    if app.active_modal == Modal::Provider {
                        // Toggle the favorite on the highlighted filtered row
                        // (falling back to the first visible row). Sending the
                        // request is enough; the backend pushes a fresh
                        // snapshot that flips the ★ next frame. As with
                        // activation, always honor the cursor — never override
                        // it with the default when the filter is empty.
                        let filtered = app.providers_filtered();
                        let target = filtered
                            .get(app.modal_index)
                            .or_else(|| filtered.first())
                            .map(|(i, _)| PROVIDERS[*i].id.to_string());
                        if let Some(id) = target {
                            let _ = app.tx.send(AgentRequest::ToggleFavorite { id });
                        }
                    }
                }
                input::InputAction::OpenModelEditor => {
                    // `e` in the picker: open the unified editor for the
                    // highlighted filtered row. The picker filter is discarded
                    // (transient search); the original chat draft stays stashed.
                    if app.active_modal == Modal::Provider {
                        let filtered = app.providers_filtered();
                        if let Some(&(idx, _)) =
                            filtered.get(app.modal_index).or_else(|| filtered.first())
                        {
                            app.editor_target = Some(idx);
                            app.editor_field = 0;
                            app.editor_key.clear();
                            app.editor_model = PROVIDERS
                                .get(idx)
                                .map(|solution| {
                                    initial_editor_model(
                                        solution,
                                        &app.current_provider,
                                        &app.current_model,
                                    )
                                })
                                .unwrap_or_default();
                            app.input.clear();
                            app.cursor_position = 0;
                            app.active_modal = Modal::ModelEditor;
                        }
                    }
                }
                input::InputAction::ModelEditorNextField => {
                    // Tab: save the focused field's buffer, swap input to the
                    // other field. The composer input line is borrowed for the
                    // focused field; the unfocused one lives in its buf.
                    if app.active_modal == Modal::ModelEditor {
                        if app.editor_field == 0 {
                            app.editor_key = std::mem::take(&mut app.input);
                            app.input = std::mem::take(&mut app.editor_model);
                            app.editor_field = 1;
                        } else {
                            app.editor_model = std::mem::take(&mut app.input);
                            app.input = std::mem::take(&mut app.editor_key);
                            app.editor_field = 0;
                        }
                        app.cursor_position = app.input.chars().count();
                    }
                }
                input::InputAction::SubmitModelEditor => {
                    if app.active_modal == Modal::ModelEditor {
                        if let Some(idx) = app.editor_target {
                            // Commit the focused field's input to its buffer first.
                            if app.editor_field == 0 {
                                app.editor_key = app.input.clone();
                            } else {
                                app.editor_model = app.input.clone();
                            }
                            if let Some(solution) = PROVIDERS.get(idx) {
                                let key = app.editor_key.trim();
                                let model = if app.editor_model.trim().is_empty() {
                                    solution.model.to_string()
                                } else {
                                    app.editor_model.trim().to_string()
                                };
                                let _ = app.tx.send(AgentRequest::SwitchProvider {
                                    provider_type: solution.id.to_string(),
                                    model,
                                    api_key: if key.is_empty() {
                                        None
                                    } else {
                                        Some(key.to_string())
                                    },
                                    base_url: None,
                                });
                            }
                            // Close to chat: restore the original draft.
                            app.input = std::mem::take(&mut app.stashed_input);
                            app.cursor_position = app.input.chars().count();
                            app.editor_target = None;
                            app.active_modal = Modal::None;
                        }
                    }
                }
                input::InputAction::Interrupt => {
                    // Mirror Ctrl+C's quit pattern: the first Esc only arms a
                    // ~2s window (and shows a toast); the second Esc within
                    // that window actually interrupts the running task.
                    if app.esc_armed_ticks > 0 {
                        app.esc_armed_ticks = 0;
                        let _ = app.tx.send(AgentRequest::Interrupt);
                    } else {
                        app.esc_armed_ticks = 20;
                    }
                }
                input::InputAction::OpenProvider => {
                    // Stash whatever the user was composing so Esc restores it;
                    // the input box is reused as the fuzzy filter while the
                    // picker is open (same pattern as HistorySearch).
                    app.stashed_input = std::mem::take(&mut app.input);
                    app.cursor_position = 0;
                    app.input_scroll = 0;
                    app.active_modal = Modal::Provider;
                    // Land the cursor on the current default so the "open picker
                    // + Enter" fast path still re-activates it. Activation always
                    // honors the highlighted row (see `ProviderPickerActivate`), so
                    // this initial position is what makes the fast path work.
                    let filtered = app.providers_filtered();
                    app.modal_index = filtered
                        .iter()
                        .position(|(i, _)| PROVIDERS[*i].id == app.provider_picker.default_id)
                        .unwrap_or(0);
                    app.suggestion_index = None;
                }
                input::InputAction::OpenHistory => {
                    // Stash whatever the user was composing so Esc restores it
                    // unchanged; the input box is reused as the fuzzy query
                    // while the modal is open (mirrors the ApiKey / Endpoint /
                    // ModelName modals that also borrow the input line).
                    app.stashed_input = std::mem::take(&mut app.input);
                    app.cursor_position = 0;
                    app.input_scroll = 0;
                    app.suggestion_index = None;
                    app.active_modal = Modal::HistorySearch;
                    // Default to the most-recent entry so an immediate Enter
                    // re-inserts the last-typed item. Empty history → 0.
                    app.modal_index = app.input_history.len().saturating_sub(1);
                }
                input::InputAction::HistoryInsert => {
                    // Enter inside the Ctrl+R modal: pull the highlighted fuzzy
                    // match out of the filtered list and drop it into the input
                    // box for further editing / sending. The message is not
                    // shipped here — the user hits Enter again to send.
                    let ranked = app.history_filtered();
                    let pick = ranked.get(app.modal_index).or_else(|| ranked.first());
                    if let Some((orig_idx, _)) = pick {
                        let original = *orig_idx;
                        app.input = app.input_history[original].clone();
                        app.cursor_position = app.input.chars().count();
                    }
                    // The selection replaces the in-progress draft, so the
                    // stash is dropped (not restored).
                    app.stashed_input.clear();
                    app.input_scroll = 0;
                    app.suggestion_index = None;
                    app.modal_index = 0;
                    app.active_modal = Modal::None;
                }
                input::InputAction::OpenCommands => {
                    // Command palette: seed the input with "/" so the existing
                    // slash-suggestion popup acts as a filterable palette.
                    if !app.input.starts_with('/') {
                        app.input = "/".to_string();
                        app.cursor_position = app.input.chars().count();
                    }
                    app.suggestion_index = None;
                }
                input::InputAction::OpenHelp => {
                    app.active_modal = Modal::Help;
                    app.modal_index = 0;
                }
                input::InputAction::OpenSession => {
                    // The session-context modal is a tabbed overview. It does
                    // not borrow the input box, so unlike Models/History there
                    // is no stash to save. Reached via the `/session` slash
                    // command (intercepted locally in input.rs, never sent to
                    // the backend). Kick off a snapshot request so the panes
                    // populate as soon as the harness replies; until then the
                    // modal renders a lightweight placeholder.
                    app.active_modal = Modal::Session;
                    app.session_tab = SessionTab::Model;
                    app.modal_index = 0;
                    app.session_scroll = 0;
                    let _ = app.tx.send(AgentRequest::QuerySessionContext);
                }
                input::InputAction::SessionTabCycle { forward } => {
                    app.session_tab = app.session_tab.cycle(forward);
                    // Reset the row cursor and scroll whenever the pane changes
                    // so a stale index from a longer list does not land past
                    // the end of the new pane's list.
                    app.modal_index = 0;
                    app.session_scroll = 0;
                }
                input::InputAction::ActivityTabCycle { forward } => {
                    app.activity_tab = app.activity_tab.cycle(forward);
                    app.activity_scroll = 0;
                }
                input::InputAction::SessionSelect { forward } => {
                    // List panes: move the selection cursor (the body scroll
                    // follows it). Read-only panes: no selection, so Up/Down
                    // scrolls the body directly.
                    let list_len = app.session_tab_list_len();
                    if list_len > 0 {
                        app.modal_index = if forward {
                            (app.modal_index + 1) % list_len
                        } else if app.modal_index == 0 {
                            list_len - 1
                        } else {
                            app.modal_index - 1
                        };
                    } else {
                        app.session_scroll = if forward {
                            app.session_scroll.saturating_add(1)
                        } else {
                            app.session_scroll.saturating_sub(1)
                        };
                    }
                }
                input::InputAction::SessionActivate => {
                    // Tab-aware mutate on the selected row. The actual request
                    // is sent through the normal agent channel; the harness
                    // replies with a fresh snapshot that re-renders the pane.
                    if let Some(req) = app.session_activate_request() {
                        let _ = app.tx.send(req);
                    }
                }
                input::InputAction::OpenSelectedSession => {
                    if let Some(session) = app.sessions_overview.get(
                        app.modal_index
                            .min(app.sessions_overview.len().saturating_sub(1)),
                    ) {
                        let id = session.id.clone();
                        app.active_modal = Modal::None;
                        app.modal_index = 0;
                        let _ = app
                            .tx
                            .send(AgentRequest::SlashCommand(format!("/session open {}", id)));
                    }
                }
                input::InputAction::DeleteSelectedSession => {
                    if let Some(session) = app.sessions_overview.get(
                        app.modal_index
                            .min(app.sessions_overview.len().saturating_sub(1)),
                    ) {
                        let id = session.id.clone();
                        let _ = app.tx.send(AgentRequest::DeleteSession { id });
                    }
                }
                input::InputAction::CloseModal => {
                    if app.active_modal == Modal::HistorySearch {
                        // The input box was borrowed as the fuzzy query; hand
                        // the in-progress draft back so Esc is a true cancel.
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        app.modal_index = 0;
                    } else if app.active_modal == Modal::Provider {
                        // The input box was borrowed as the fuzzy filter; hand
                        // the in-progress draft back so Esc cancels cleanly.
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        app.modal_index = 0;
                    } else if app.active_modal == Modal::ModelEditor {
                        // Cancel the editor: discard its fields and return to
                        // the picker with a fresh (empty) filter. The original
                        // chat draft stays in stashed_input for when the picker
                        // itself closes.
                        app.editor_target = None;
                        app.input.clear();
                        app.cursor_position = 0;
                        app.active_modal = Modal::Provider;
                    }
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_message_idx = None;
                        app.tool_detail_scroll = 0;
                    }
                    app.active_modal = Modal::None;
                }
                input::InputAction::ScrollUp => {
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_scroll = app.tool_detail_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = app.activity_scroll.saturating_sub(1);
                    } else {
                        // While a permission sheet is open the transcript stays
                        // scrollable, so the wheel / page keys drive the
                        // conversation behind it, not the sheet's own body.
                        app.follow_bottom = false;
                        app.pin_summary_line = None;
                        // Mouse wheel tick = 4 lines, not 1, so scrolling feels fast
                        // and responsive instead of crawling line-by-line.
                        app.scroll = app.scroll.saturating_sub(4);
                    }
                }
                input::InputAction::ScrollDown => {
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_scroll = app.tool_detail_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = app.activity_scroll.saturating_add(1);
                    } else {
                        app.pin_summary_line = None;
                        app.scroll = app.scroll.saturating_add(4).min(app.max_scroll);
                        if app.scroll >= app.max_scroll {
                            app.follow_bottom = true;
                        }
                    }
                }
                input::InputAction::ScrollPageUp => {
                    app.follow_bottom = false;
                    app.pin_summary_line = None;
                    // Leave one line of overlap so the reader keeps context.
                    let step = app.view_height.saturating_sub(1).max(1);
                    app.scroll = app.scroll.saturating_sub(step);
                }
                input::InputAction::ScrollPageDown => {
                    app.pin_summary_line = None;
                    let step = app.view_height.saturating_sub(1).max(1);
                    app.scroll = app.scroll.saturating_add(step).min(app.max_scroll);
                    if app.scroll >= app.max_scroll {
                        app.follow_bottom = true;
                    }
                }
                input::InputAction::ScrollTop => {
                    app.follow_bottom = false;
                    app.pin_summary_line = None;
                    app.scroll = 0;
                }
                input::InputAction::ScrollBottom => {
                    app.pin_summary_line = None;
                    app.scroll = app.max_scroll;
                    app.follow_bottom = true;
                }
                input::InputAction::PermissionDetailsUp => {
                    app.permission_scroll = app.permission_scroll.saturating_sub(1);
                }
                input::InputAction::PermissionDetailsDown => {
                    app.permission_scroll = app
                        .permission_scroll
                        .saturating_add(1)
                        .min(app.permission_max_scroll);
                }
                input::InputAction::CopySelection => {
                    if let Some(text) = extract_selection_text(
                        &app.selection,
                        app.focused_messages(),
                        &app.input,
                        &app.layout_map,
                    ) {
                        clipboard_ops::spawn_clipboard_copy(&copy_tx, copy_pending.clone(), text);
                    }
                }
                input::InputAction::CtrlC => {
                    if let Some(text) = extract_selection_text(
                        &app.selection,
                        app.focused_messages(),
                        &app.input,
                        &app.layout_map,
                    ) {
                        clipboard_ops::spawn_clipboard_copy(&copy_tx, copy_pending.clone(), text);
                    } else if app.active_modal == Modal::HistorySearch {
                        // Cancel the fuzzy query: restore the in-progress draft
                        // the user was composing before Ctrl+R.
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        app.modal_index = 0;
                        app.active_modal = Modal::None;
                    } else if app.active_modal != Modal::None
                        && app.active_modal != Modal::Permission
                    {
                        app.active_modal = Modal::None;
                    } else if app.in_side_view {
                        // `/btw` side view: Ctrl+C leaves the side
                        // conversation (ADR-0017), mirroring Esc. Slotted
                        // after modal-close so an open overlay still wins.
                        app.exit_side_view();
                        let _ = app.tx.send(AgentRequest::ExitSideView);
                    } else if runtime.is_responding.load(Ordering::SeqCst) {
                        let _ = app.tx.send(AgentRequest::Interrupt);
                    } else if !app.input.is_empty() {
                        app.input.clear();
                        app.cursor_position = 0;
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        // Clearing the input also arms the quit window so
                        // the chain is exactly two presses total (clear,
                        // then quit). The combined toast says both what
                        // just happened and what the next Ctrl+C will do,
                        // removing the old "silent clear → user can't tell
                        // if the next press will quit or do something else"
                        // ambiguity. Pending-image reminders skip their
                        // per-frame refresh while the quit window is armed
                        // so this toast keeps the floor.
                        app.copy_toast_message = "input cleared — Ctrl+C again to exit".to_string();
                        app.copy_toast_failed = false;
                        app.copy_toast_until = Some(
                            std::time::Instant::now() + std::time::Duration::from_millis(2000),
                        );
                        app.ctrl_c_armed_ticks = 20;
                    } else if app.ctrl_c_armed_ticks > 0 {
                        return Ok(());
                    } else {
                        // Arm a ~2s window in which a second Ctrl+C quits.
                        app.ctrl_c_armed_ticks = 20;
                    }
                }
                input::InputAction::ToggleToolSteps => {
                    // Read the target state from the focused view (a snapshot
                    // clone), then apply to the live messages.
                    let expand = app.focused_messages().iter().any(|message| {
                        !message.is_subagent_task() && message.tool_step_expanded() == Some(false)
                    });
                    let mut messages = runtime.messages.lock().await;
                    for message in focused_messages_mut(&mut messages, &app.focus_stack) {
                        // Subagent task steps are navigated, not expanded.
                        // This is a user bulk action → pin each step so the
                        // choice survives later lifecycle transitions.
                        if !message.is_subagent_task() {
                            message.pin_tool_step_expanded(expand);
                        }
                    }
                    drop(messages);
                    // Persist the choice as the global density so new tool steps
                    // created mid-turn also respect it (ADR-0001 Step 8).
                    app.tool_density.store(expand, Ordering::SeqCst);
                    app.selection = SelectionState::None;
                }
                input::InputAction::FocusNextTarget => {
                    app.focus_interactive_target(1);
                }
                input::InputAction::FocusPrevTarget => {
                    app.focus_interactive_target(-1);
                }
                input::InputAction::EnterBrowseZone { backward } => {
                    // Hand keyboard focus from the input box over to the
                    // conversation stream. Direction picks the closest step:
                    // forward (Tab) selects the first one, backward (Shift+Tab)
                    // selects the last one.
                    app.focus_zone = input::FocusZone::Browse;
                    let dir: i8 = if backward { -1 } else { 1 };
                    app.focus_interactive_target(dir);
                }
                input::InputAction::ReturnToComposeZone => {
                    app.focus_zone = input::FocusZone::Compose;
                }
                input::InputAction::ActivateFocusedTarget => {
                    if let Some(target) = app.focused_target {
                        match target.kind {
                            InteractiveTargetKind::ToolStep => {
                                let mut messages = runtime.messages.lock().await;
                                let enter_id = resolve_focused_mut(
                                    &mut messages,
                                    &app.focus_stack,
                                    target.message_idx,
                                )
                                .and_then(|message| {
                                    if message.is_subagent_task() {
                                        message.tool_step_call_id().map(String::from)
                                    } else {
                                        None
                                    }
                                });
                                if let Some(id) = enter_id {
                                    drop(messages);
                                    app.enter_subagent(id);
                                } else {
                                    // Open the full-output detail overlay instead
                                    // of the inline expand/collapse (the latter is
                                    // the cramped UX the redesign replaces). The
                                    // bulk `ctrl+t` toggle still inline-expands
                                    // every step if desired.
                                    drop(messages);
                                    app.tool_detail_message_idx = Some(target.message_idx);
                                    app.tool_detail_scroll = 0;
                                    app.active_modal = Modal::ToolStepDetail;
                                }
                            }
                            InteractiveTargetKind::Thinking => {
                                let mut messages = runtime.messages.lock().await;
                                let toggled =
                                    app.toggle_step_pinned(&mut messages, target.message_idx);
                                drop(messages);
                                if toggled {
                                    app.selection = SelectionState::None;
                                }
                            }
                        }
                    }
                }
                input::InputAction::Paste => {
                    // Ctrl+V: read the system clipboard off the event loop.
                    // The result is delivered back through `paste_rx` and
                    // applied on a later frame (image -> attach, text -> insert).
                    if app.active_modal == Modal::None {
                        clipboard_ops::spawn_clipboard_paste(&paste_tx);
                    }
                }
                input::InputAction::BracketedPaste(text) => {
                    // Terminal-level paste (bracketed paste mode). The payload
                    // is already in hand, so route it directly through the same
                    // chip-or-inline logic as Ctrl+V without an async hop.
                    clipboard_ops::apply_clipboard_paste(app, clipboard::ClipboardRead::Text(text));
                }
                input::InputAction::ExitSubAgent => {
                    app.exit_subagent();
                }
                input::InputAction::ExitSideView => {
                    // `/btw`: return to the primary transcript (ADR-0017).
                    // Optimistically flip the view for snappiness and tell the
                    // harness to tear down the live side session; its
                    // `SideViewClosed` reply is a backstop in case this fires
                    // twice (Esc then Ctrl+C).
                    if app.in_side_view {
                        app.exit_side_view();
                        let _ = app.tx.send(AgentRequest::ExitSideView);
                    }
                }
                input::InputAction::PrevSibling => {
                    app.cycle_sibling(-1);
                }
                input::InputAction::NextSibling => {
                    app.cycle_sibling(1);
                }
                input::InputAction::InsertChar(c) => {
                    // Already handled by process_event mutating app.input
                    let _ = c;
                    app.suggestion_index = None;
                    // The user is editing again, so live completions are
                    // once again useful — clear the Enter-commit dismissal.
                    app.completion_dismissed = false;
                    // Reconcile attachments: if the user typed inside a chip
                    // (breaking its syntax) the backing staged entry must be
                    // dropped, and surviving chips relabeled.
                    app.reconcile_attachments();
                }
                input::InputAction::Backspace => {
                    app.suggestion_index = None;
                    app.completion_dismissed = false;
                    // Reconcile attachments: a chip-aware backspace has
                    // already spliced the chip out of `app.input`; this
                    // drops the orphaned entry from `pending_images` /
                    // `pending_text_pastes` and relabels survivors.
                    app.reconcile_attachments();
                }
                input::InputAction::SuggestNext => {
                    let count = app.completions().len();
                    if count > 0 {
                        let next = match app.suggestion_index {
                            Some(i) => (i + 1) % count,
                            None => 0,
                        };
                        app.suggestion_index = Some(next);
                    }
                }
                input::InputAction::SuggestPrev => {
                    let count = app.completions().len();
                    if count > 0 {
                        let prev = match app.suggestion_index {
                            Some(i) => {
                                if i == 0 {
                                    count - 1
                                } else {
                                    i - 1
                                }
                            }
                            None => count - 1,
                        };
                        app.suggestion_index = Some(prev);
                    }
                }
                input::InputAction::AcceptSuggestion(idx_str) => {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        app.accept_completion(idx);
                    }
                }
                input::InputAction::CommitSuggestion(idx_str) => {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        app.accept_completion(idx);
                    }
                    // Enter "finishes" the completion: drop the highlight
                    // and latch the dismissal flag so the popup stays
                    // hidden until the user edits the input again. The
                    // suppression is re-checked at the top of each frame
                    // against `app.completion_dismissed`.
                    app.suggestion_index = None;
                    app.completion_dismissed = true;
                }
                input::InputAction::CloseCompletion => {
                    // Esc dismisses the popup without accepting anything.
                    // Same latch as Enter-commit so the popup stays hidden
                    // until the next edit clears `completion_dismissed`.
                    app.suggestion_index = None;
                    app.completion_dismissed = true;
                }
                input::InputAction::HistoryPrev => {
                    if !app.input_history.is_empty() {
                        let new_idx = match app.history_index {
                            Some(i) => {
                                if i == 0 {
                                    0
                                } else {
                                    i - 1
                                }
                            }
                            None => app.input_history.len() - 1,
                        };
                        app.history_index = Some(new_idx);
                        app.input = app.input_history[new_idx].clone();
                        app.cursor_position = app.input.chars().count();
                    }
                }
                input::InputAction::RecallQueued => {
                    // Pop the most-recently-queued message (LIFO undo),
                    // remove its visual marker from the transcript, and
                    // load its text + images back into the composer so the
                    // user can edit and resend. The actual state mutation
                    // lives on [`App::recall_queued`] so it is unit-testable
                    // against a plain transcript Vec.
                    let mut messages = runtime.messages.lock().await;
                    app.recall_queued(&mut messages);
                }
                input::InputAction::HistoryNext => {
                    if let Some(i) = app.history_index {
                        if i + 1 < app.input_history.len() {
                            let new_idx = i + 1;
                            app.history_index = Some(new_idx);
                            app.input = app.input_history[new_idx].clone();
                            app.cursor_position = app.input.chars().count();
                        } else {
                            app.history_index = None;
                            app.input = String::new();
                            app.cursor_position = 0;
                        }
                    }
                }
                input::InputAction::ModalUp => match app.active_modal {
                    Modal::Provider => {
                        // Walk the fuzzy-filtered list, not the raw catalog, so
                        // the cursor never lands on a hidden row (same rule as
                        // the history-search modal).
                        let count = app.providers_filtered().len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::HistorySearch => {
                        // Up/Down walk the fuzzy-filtered list, not the raw
                        // history, so the cursor never lands on an entry the
                        // user cannot actually see or select.
                        let count = app.history_filtered().len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::Permission => {
                        let count = if app.permission_confirm_always { 2 } else { 4 };
                        app.modal_index = if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::Sessions => {
                        let count = app.sessions_overview.len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::Help
                    | Modal::ToolStepDetail
                    | Modal::Question
                    | Modal::ModelEditor
                    | Modal::Session
                    | Modal::Activity
                    | Modal::None => {}
                },
                input::InputAction::ModalDown => match app.active_modal {
                    Modal::Provider => {
                        let count = app.providers_filtered().len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::HistorySearch => {
                        let count = app.history_filtered().len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Permission => {
                        let count = if app.permission_confirm_always { 2 } else { 4 };
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Sessions => {
                        let count = app.sessions_overview.len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Help
                    | Modal::ToolStepDetail
                    | Modal::Question
                    | Modal::ModelEditor
                    | Modal::Session
                    | Modal::Activity
                    | Modal::None => {}
                },
                input::InputAction::QuestionUp => {
                    if app.active_modal == Modal::Question {
                        if let Some(ref request) = app.pending_question {
                            let current = app.question_current;
                            let options_count = request.questions[current].options.len() + 1;
                            app.modal_index = if app.modal_index == 0 {
                                options_count - 1
                            } else {
                                app.modal_index - 1
                            };
                        }
                    }
                }
                input::InputAction::QuestionDown => {
                    if app.active_modal == Modal::Question {
                        if let Some(ref request) = app.pending_question {
                            let current = app.question_current;
                            let options_count =
                                (request.questions[current].options.len() + 1).max(1);
                            app.modal_index = (app.modal_index + 1) % options_count;
                        }
                    }
                }
                input::InputAction::QuestionToggle => {
                    if app.active_modal == Modal::Question {
                        if let Some(ref request) = app.pending_question {
                            let q = app.question_current;
                            let i = app.modal_index;
                            let multi = request.questions[q].multi_select;
                            let selected = &mut app.question_selected[q];
                            if multi {
                                if let Some(pos) = selected.iter().position(|&x| x == i) {
                                    selected.remove(pos);
                                } else {
                                    selected.push(i);
                                    selected.sort();
                                }
                            } else {
                                selected.clear();
                                selected.push(i);
                            }
                        }
                    }
                }
                input::InputAction::QuestionSelect(n) => {
                    if app.active_modal == Modal::Question {
                        if let Some(ref request) = app.pending_question {
                            let q = app.question_current;
                            let total_options = request.questions[q].options.len() + 1;
                            if n > 0 && n <= total_options {
                                app.modal_index = n - 1;
                                let multi = request.questions[q].multi_select;
                                let selected = &mut app.question_selected[q];
                                if multi {
                                    if let Some(pos) = selected.iter().position(|&x| x == n - 1) {
                                        selected.remove(pos);
                                    } else {
                                        selected.push(n - 1);
                                        selected.sort();
                                    }
                                } else {
                                    selected.clear();
                                    selected.push(n - 1);
                                }
                            }
                        }
                    }
                }
                input::InputAction::QuestionSubmit => {
                    if app.active_modal == Modal::Question {
                        if let Some(request) = app.pending_question.take() {
                            let request_id = request.id.clone();
                            let answers: Vec<Vec<String>> = request
                                .questions
                                .iter()
                                .enumerate()
                                .map(|(q_idx, q)| {
                                    let other_index = q.options.len();
                                    let other_text = app
                                        .question_other_text
                                        .get(q_idx)
                                        .cloned()
                                        .unwrap_or_default();
                                    app.question_selected
                                        .get(q_idx)
                                        .map(|sel| {
                                            sel.iter()
                                                .map(|&opt_idx| {
                                                    if opt_idx == other_index {
                                                        if other_text.is_empty() {
                                                            "Other".to_string()
                                                        } else {
                                                            other_text.clone()
                                                        }
                                                    } else {
                                                        q.options
                                                            .get(opt_idx)
                                                            .map(|o| o.label.clone())
                                                            .unwrap_or_default()
                                                    }
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default()
                                })
                                .collect();
                            let parent_call_id = runtime
                                .subagent_question_parent
                                .lock()
                                .await
                                .remove(&request_id);
                            let _ = app.tx.send(AgentRequest::UserQuestionReply {
                                request_id: request_id.clone(),
                                answers,
                                parent_call_id,
                            });
                            let mut queue = runtime.pending_question.lock().await;
                            queue.retain(|r| r.id != request_id);
                            app.pending_question = queue.front().cloned();
                            if app.pending_question.is_none() {
                                app.active_modal = Modal::None;
                            }
                            app.modal_index = 0;
                            app.question_current = 0;
                            app.question_selected.clear();
                            app.question_other_text.clear();
                        }
                    }
                }
                input::InputAction::QuestionCancel => {
                    if app.active_modal == Modal::Question {
                        if let Some(request) = app.pending_question.take() {
                            let request_id = request.id;
                            let parent_call_id = runtime
                                .subagent_question_parent
                                .lock()
                                .await
                                .remove(&request_id);
                            let _ = app.tx.send(AgentRequest::UserQuestionReply {
                                request_id: request_id.clone(),
                                answers: Vec::new(),
                                parent_call_id,
                            });
                            let mut queue = runtime.pending_question.lock().await;
                            queue.retain(|r| r.id != request_id);
                            app.pending_question = queue.front().cloned();
                            app.active_modal = Modal::None;
                            app.modal_index = 0;
                            app.question_current = 0;
                            app.question_selected.clear();
                            app.question_other_text.clear();
                        }
                    }
                }
                input::InputAction::QuestionInsertChar(c) => {
                    if app.active_modal == Modal::Question {
                        if let Some(ref request) = app.pending_question {
                            let q = app.question_current;
                            let other_index = request.questions[q].options.len();
                            if app.modal_index == other_index {
                                if let Some(text) = app.question_other_text.get_mut(q) {
                                    text.push(c);
                                }
                            }
                        }
                    }
                }
                input::InputAction::QuestionBackspace => {
                    if app.active_modal == Modal::Question {
                        if let Some(ref request) = app.pending_question {
                            let q = app.question_current;
                            let other_index = request.questions[q].options.len();
                            if app.modal_index == other_index {
                                if let Some(text) = app.question_other_text.get_mut(q) {
                                    text.pop();
                                }
                            }
                        }
                    }
                }
                input::InputAction::PermissionSubmit => {
                    if app.permission_confirm_always {
                        // Confirm-always sub-step: index 0 = Confirm, 1 = Cancel.
                        if app.modal_index == 1 {
                            app.permission_confirm_always = false;
                            app.modal_index = 1;
                            break 'event_batch;
                        }
                        // index 0: fall through to send Always.
                    } else {
                        // "Details" (index 3): expand/collapse the body without
                        // deciding, so the user can review before acting.
                        if app.modal_index == 3 {
                            app.permission_show_details = !app.permission_show_details;
                            app.permission_scroll = 0;
                            break 'event_batch;
                        }
                        // "Always allow" (index 1): gate behind a confirm step.
                        if app.modal_index == 1 {
                            app.permission_confirm_always = true;
                            app.permission_show_details = false;
                            app.modal_index = 0;
                            break 'event_batch;
                        }
                    }
                    if let Some(request) = app.pending_permission.take() {
                        let decision = if app.permission_confirm_always {
                            PermissionDecision::Always
                        } else {
                            // index 0 = Allow once, index 2 = Reject.
                            match app.modal_index {
                                0 => PermissionDecision::Once,
                                _ => PermissionDecision::Reject,
                            }
                        };
                        let request_id = request.id;
                        let parent_call_id = runtime
                            .subagent_permission_parent
                            .lock()
                            .await
                            .remove(&request_id);
                        let _ = app.tx.send(AgentRequest::PermissionReply {
                            request_id: request_id.clone(),
                            decision,
                            parent_call_id,
                        });
                        if decision == PermissionDecision::Reject {
                            // A rejection aborts the turn: resolve every other
                            // queued request too, otherwise their tool futures
                            // stay blocked and the batch deadlocks.
                            let queued: Vec<PermissionRequest> =
                                runtime.pending_permission.lock().await.drain(..).collect();
                            let mut parents = runtime.subagent_permission_parent.lock().await;
                            for pending in queued {
                                let parent_call_id = parents.remove(&pending.id);
                                let _ = app.tx.send(AgentRequest::PermissionReply {
                                    request_id: pending.id,
                                    decision: PermissionDecision::Reject,
                                    parent_call_id,
                                });
                            }
                            app.pending_permission = None;
                            app.active_modal = Modal::None;
                        } else {
                            // Drop the request we just answered and surface the
                            // next one (if any) so the sheet hands off without
                            // flashing the composer for a frame.
                            let mut queue = runtime.pending_permission.lock().await;
                            queue.retain(|r| r.id != request_id);
                            app.pending_permission = queue.front().cloned();
                            drop(queue);
                            if app.pending_permission.is_none() {
                                app.active_modal = Modal::None;
                            }
                        }
                        app.modal_index = 0;
                        app.permission_scroll = 0;
                        app.permission_max_scroll = 0;
                        app.permission_confirm_always = false;
                        app.permission_show_details = false;
                    }
                }
                input::InputAction::PermissionReject => {
                    // Rejecting aborts the turn; resolve every queued request
                    // so the concurrent tool batch can finish.
                    let queued: Vec<PermissionRequest> =
                        runtime.pending_permission.lock().await.drain(..).collect();
                    app.pending_permission = None;
                    app.active_modal = Modal::None;
                    app.modal_index = 0;
                    app.permission_confirm_always = false;
                    app.permission_show_details = false;
                    let mut parents = runtime.subagent_permission_parent.lock().await;
                    for pending in queued {
                        let parent_call_id = parents.remove(&pending.id);
                        let _ = app.tx.send(AgentRequest::PermissionReply {
                            request_id: pending.id,
                            decision: PermissionDecision::Reject,
                            parent_call_id,
                        });
                    }
                }
                input::InputAction::PermissionBack => {
                    app.permission_confirm_always = false;
                    app.modal_index = 1;
                }
                input::InputAction::SelectionStart { x, y } => {
                    // Click-to-dismiss: while a dismissable overlay modal is
                    // open, the full-screen backdrop owns the click — a press
                    // outside the panel closes the modal (mirroring Esc), and a
                    // press inside is a no-op (these info modals have no click
                    // targets yet). Either way the click is consumed so it does
                    // not also fall through to the transcript behind the
                    // backdrop. Modals that need their own restore path
                    // (Provider / ModelEditor / HistorySearch) report no rect
                    // and are skipped here, so a stray click never discards an
                    // in-progress filter or API key.
                    if let Some(r) = app.modal_rect {
                        let inside =
                            r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height;
                        if !inside {
                            app.active_modal = Modal::None;
                        }
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.activity_rect.is_some_and(|r| {
                        r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                    }) {
                        app.active_modal = Modal::Activity;
                        app.activity_tab = ActivityTab::Activity;
                        app.modal_index = 0;
                        app.activity_scroll = 0;
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.sticky_rect.is_some_and(|r| {
                        // Sticky pinned step header: collapse it on click.
                        r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                    }) {
                        if let Some(mi) = app.sticky_step {
                            let mut messages = runtime.messages.lock().await;
                            app.focused_target =
                                app.focused_messages().get(mi).and_then(|message| {
                                    if message.is_thinking() {
                                        Some(InteractiveTarget::thinking(mi))
                                    } else if message.is_tool_step() || message.is_subagent_task() {
                                        Some(InteractiveTarget::tool_step(mi))
                                    } else {
                                        None
                                    }
                                });
                            app.toggle_step_pinned(&mut messages, mi);
                            drop(messages);
                        }
                        // Activating a step via click implies keyboard focus
                        // follows it as well.
                        app.focus_zone = input::FocusZone::Browse;
                        app.selection = SelectionState::None;
                        app.drag.cancel();
                    } else if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        if cursor.message_idx == crate::tui::render::INPUT_MSG_IDX {
                            // Click inside the live input box: hand keyboard
                            // focus back to the prompt so the next keypress
                            // edits rather than navigating steps.
                            app.focus_zone = input::FocusZone::Compose;
                            app.focused_target = None;
                            app.selection = SelectionState::start_range(cursor);
                            app.drag.start(cursor);
                        } else if let Some((mi, kind)) = step_interaction::summary_at(&cursor) {
                            // Clicked a step summary: navigate into a subagent
                            // task, otherwise toggle that step's disclosure.
                            app.focused_target = Some(kind.focus_target(mi));
                            let mut messages = runtime.messages.lock().await;
                            match kind {
                                step_interaction::StepKind::ToolStep => {
                                    let enter_id =
                                        resolve_focused_mut(&mut messages, &app.focus_stack, mi)
                                            .and_then(|message| {
                                                if message.is_subagent_task() {
                                                    message.tool_step_call_id().map(String::from)
                                                } else {
                                                    None
                                                }
                                            });
                                    if let Some(id) = enter_id {
                                        drop(messages);
                                        app.enter_subagent(id);
                                    } else {
                                        app.toggle_step_pinned(&mut messages, mi);
                                        drop(messages);
                                    }
                                }
                                step_interaction::StepKind::Thinking => {
                                    app.toggle_step_pinned(&mut messages, mi);
                                    drop(messages);
                                }
                            }
                            app.focus_zone = input::FocusZone::Browse;
                            app.selection = SelectionState::None;
                            app.drag.cancel();
                        } else {
                            // Inside a table cell, a press places the cursor
                            // and starts a drag confined to that cell: the
                            // selection can roam across the cell's wrapped
                            // lines but never crosses `│` borders. A plain
                            // click (no drag) leaves nothing selected.
                            if let Some((mi, bi, cell)) = app.layout_map.table_cell_at(x, y) {
                                app.selection = SelectionState::start_range(cursor);
                                app.drag.start_in_cell(cursor, (mi, bi, cell));
                            } else {
                                app.selection = SelectionState::start_range(cursor);
                                app.drag.start(cursor);
                            }
                            app.focused_target = None;
                            // Clicking anywhere in the conversation content
                            // hands keyboard focus to the stream (Browse), so
                            // the click location always determines the zone.
                            app.focus_zone = input::FocusZone::Browse;
                        }
                    } else {
                        // The click missed every registered region. If it
                        // still lands inside the transcript content rect —
                        // e.g. on a gap row between messages or a spacing row
                        // inside an expanded step — treat it as a browse-focus
                        // gesture: switch keyboard focus to the stream so the
                        // user can immediately navigate with the keyboard,
                        // without starting a text selection (there is no
                        // cursor to anchor one). Clicks in the outer gutters
                        // or below all content stay inert.
                        if app.layout_map.transcript_content_rect().is_some_and(|r| {
                            r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                        }) {
                            app.focus_zone = input::FocusZone::Browse;
                        }
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    }
                }
                input::InputAction::RightClick { x, y } => {
                    // Right-click on a tool-step summary opens the full-output
                    // detail overlay. For permission-denied steps this is the
                    // fastest way to surface the "Permission denied" message
                    // and the terminated-turn feedback.
                    if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        if let Some((mi, step_interaction::StepKind::ToolStep)) =
                            step_interaction::summary_at(&cursor)
                        {
                            app.focused_target = Some(InteractiveTarget::tool_step(mi));
                            app.tool_detail_message_idx = Some(mi);
                            app.tool_detail_scroll = 0;
                            app.active_modal = Modal::ToolStepDetail;
                        }
                    }
                    app.selection = SelectionState::None;
                    app.drag.cancel();
                }
                input::InputAction::SelectionUpdate { x, y } => {
                    // Keep a cell-confined drag from leaking past `│` borders.
                    let (x, y) = if let Some(cell) = app.drag.cell_constraint {
                        app.layout_map
                            .clamp_to_table_cell(cell, x, y)
                            .unwrap_or((x, y))
                    } else {
                        (x, y)
                    };
                    if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        app.selection.update_head(cursor);
                    }
                }
                input::InputAction::SelectionEnd => {
                    app.drag.end();
                    // If selection is empty, clear it
                    if let Some((a, b)) = app.selection.normalized_range() {
                        if a == b {
                            app.selection = SelectionState::None;
                        }
                    }
                }
                input::InputAction::SelectBlock { x, y } => {
                    if let Some((mi, bi)) = input::resolve_block(&app.layout_map, x, y) {
                        app.selection = SelectionState::Block {
                            message_idx: mi,
                            block_idx: bi,
                        };
                    }
                }
                input::InputAction::Hover { x, y } => {
                    // Every step summary (tool step, subagent task, reasoning
                    // trace) carries the same hover affordance. When the pointer
                    // rests on one — either the inline summary or the sticky
                    // pinned variant — record its message index so the next draw
                    // lights it up to the intermediate hover tone; otherwise
                    // clear it.
                    if app.sticky_rect.is_some_and(|r| {
                        r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                    }) {
                        if let Some(mi) = app.sticky_step {
                            let is_step = runtime
                                .messages
                                .lock()
                                .await
                                .get(mi)
                                .map(|m| {
                                    m.is_thinking() || m.is_tool_step() || m.is_subagent_task()
                                })
                                .unwrap_or(false);
                            app.hovered_step = is_step.then_some(mi);
                        }
                    } else if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        app.hovered_step = step_interaction::hovered_summary(&cursor);
                    } else {
                        app.hovered_step = None;
                    }
                }
            }
        }
    }
}

pub(super) fn tool_activity_status(name: &str) -> &'static str {
    match name {
        "read_file" | "read_image" | "list_dir" | "use_skill" => "exploring",
        "grep" => "searching codebase",
        "write_file" | "edit_file" => "making edits",
        "bash" => "running command",
        name if name.starts_with("mcp__") => "using MCP",
        _ => "using tool",
    }
}

/// Snapshot the currently active provider id and model so a freshly created
/// message can be attributed to the model that produced it. The listener keeps
/// these in sync with the harness via `ProviderSwitched` and the initial
/// selection, so live messages stay traceable just like restored ones.
pub(super) async fn attribution(
    provider: &Arc<Mutex<String>>,
    model: &Arc<Mutex<String>>,
) -> (String, String) {
    (provider.lock().await.clone(), model.lock().await.clone())
}

pub(super) fn compact_retry_reason(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or(message).trim();
    let mut chars = first_line.chars();
    let prefix = chars.by_ref().take(56).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", prefix)
    } else {
        prefix
    }
}

/// Resolve a mutable reference to the message at index `mi` within the
/// currently focused view: the root conversation when the focus stack is empty,
/// or the focused subagent task's child stream otherwise. Selection and layout
/// indices are recorded against whichever slice was rendered, so mutations must
/// resolve through the same context.
pub(super) fn resolve_focused_mut<'a>(
    messages: &'a mut [TranscriptMessage],
    focus_stack: &[String],
    mi: usize,
) -> Option<&'a mut TranscriptMessage> {
    let Some(current) = focus_stack.last() else {
        return messages.get_mut(mi);
    };
    let task_idx = messages.iter().position(|message| {
        message.is_subagent_task() && message.tool_step_call_id() == Some(current.as_str())
    })?;
    messages[task_idx].subagent_children_mut()?.get_mut(mi)
}

/// Iterate mutable messages in the currently focused view (the root
/// conversation, or the focused subagent task's child stream) for bulk
/// expand/collapse operations. Callers filter by kind as needed.
pub(super) fn focused_messages_mut<'a>(
    messages: &'a mut [TranscriptMessage],
    focus_stack: &[String],
) -> Box<dyn Iterator<Item = &'a mut TranscriptMessage> + 'a> {
    match focus_stack.last() {
        None => Box::new(messages.iter_mut()),
        Some(current) => {
            let task_idx = messages.iter().position(|message| {
                message.is_subagent_task() && message.tool_step_call_id() == Some(current.as_str())
            });
            match task_idx {
                Some(idx) => match messages[idx].subagent_children_mut() {
                    Some(children) => Box::new(children.iter_mut()),
                    None => Box::new(std::iter::empty()),
                },
                None => Box::new(std::iter::empty()),
            }
        }
    }
}

/// Extract selected text from either transcript messages or the live input box,
/// depending on which the semantic selection covers.
pub(super) fn extract_selection_text(
    sel: &SelectionState,
    messages: &[crate::tui::document::TranscriptMessage],
    input: &str,
    layout_map: &crate::tui::layout::LayoutMap,
) -> Option<String> {
    let on_input = match sel {
        SelectionState::None => false,
        SelectionState::Block { message_idx, .. } => {
            *message_idx == crate::tui::render::INPUT_MSG_IDX
        }
        SelectionState::TableCell { message_idx, .. } => {
            *message_idx == crate::tui::render::INPUT_MSG_IDX
        }
        SelectionState::Range { anchor, head } => {
            anchor.message_idx == crate::tui::render::INPUT_MSG_IDX
                && head.message_idx == crate::tui::render::INPUT_MSG_IDX
        }
    };
    if !on_input {
        return get_selected_text(sel, messages, &|mi, bi| layout_map.table_grid(mi, bi));
    }
    match sel {
        SelectionState::Block { .. } => Some(input.to_string()),
        SelectionState::Range { anchor, head } => {
            let (start, end) = if anchor.byte_offset <= head.byte_offset {
                (anchor.byte_offset, head.byte_offset)
            } else {
                (head.byte_offset, anchor.byte_offset)
            };
            let start = floor_char_boundary(input, start);
            let end = inclusive_end(input, end);
            (start < end).then(|| input[start..end].to_string())
        }
        _ => None,
    }
}

fn initial_editor_model(
    solution: &crate::tui::ProviderPreset,
    current_provider: &str,
    current_model: &str,
) -> String {
    if current_provider == solution.id {
        return current_model.to_string();
    }
    solution.model.to_string()
}

pub(super) fn display_status(
    loop_status: &str,
    activity: &str,
    awaiting_permission: bool,
) -> String {
    let activity = if awaiting_permission {
        "awaiting permission"
    } else {
        activity
    };
    match (loop_status, activity) {
        ("idle", "") => "idle".to_string(),
        ("idle", activity) => activity.to_string(),
        // "running" is implied by the activity bar's spinner + live status,
        // so it would be redundant noise ahead of the status. Drop
        // it and show the activity alone — but fall back to "preparing" when
        // no specific activity has landed yet (the gap between turn start
        // and the first `AgentResponse::Activity`), so the activity bar
        // always has a non-empty label to anchor the breathing dot against.
        ("running", "") => "preparing".to_string(),
        ("running", activity) => activity.to_string(),
        (loop_status, "") => loop_status.to_string(),
        (loop_status, activity) => format!("{} · {}", loop_status, activity),
    }
}
