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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crossterm::event;
use neenee_tui::Terminal;
use tokio::sync::mpsc;

use neenee_core::{
    AgentRequest, HarnessSnapshot, ParentStatus, PermissionDecision, PermissionRequest,
    ProviderPickerSnapshot, Role, SessionOverview, TodoList, UserQuestionRequest,
};

use crate::tui::clipboard;
use crate::tui::clipboard_ops;
use crate::tui::completion::CompletionKind;
use crate::tui::composer_attachments;
use crate::tui::document::{TranscriptMessage, UserMessageOrigin};
use crate::tui::input::{self};
use crate::tui::interaction::{self, ClickTarget};
use crate::tui::layout::{InteractiveTarget, InteractiveTargetKind, LayoutMap};
use crate::tui::render;
use crate::tui::selection::{
    CellDragInfo, SelectionState, floor_grapheme_boundary, get_selected_text,
    inclusive_grapheme_end,
};
use crate::tui::step_interaction::StepKind;
use crate::tui::{ActivityTab, App, Modal, PROVIDERS, Recess};

use neenee_core::AgentResponse;
use tokio::sync::{Mutex, broadcast};

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

async fn handle_permission_submit(app: &mut App, runtime: &UiRuntime) {
    if app.permission_confirm_always {
        // Confirm-always sub-step: index 0 = Confirm, 1 = Cancel.
        if app.modal_index == 1 {
            app.permission_confirm_always = false;
            app.modal_index = 1;
            return;
        }
        // index 0: fall through to send Always.
    } else {
        // "Details" (index 3): expand/collapse the body without deciding, so
        // the user can review before acting.
        if app.modal_index == 3 {
            app.permission_show_details = !app.permission_show_details;
            app.permission_scroll = 0;
            return;
        }
        // "Always allow" (index 1): gate behind a confirm step.
        if app.modal_index == 1 {
            app.permission_confirm_always = true;
            app.permission_show_details = false;
            app.modal_index = 0;
            return;
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
            // A rejection aborts the turn: resolve every other queued request
            // too, otherwise their tool futures stay blocked and the batch
            // deadlocks.
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
            // Drop the request we just answered and surface the next one (if
            // any) so the sheet hands off without flashing the composer for a
            // frame.
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

pub(super) async fn run_app_loop(
    terminal: &mut Terminal<std::io::Stdout>,
    app: &mut App,
    runtime: UiRuntime,
    session: Arc<neenee_store::session::SessionStore>,
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
            app.unattended = harness.unattended;
            app.activity_status = runtime.activity_status.lock().await.clone();
            app.session_context = runtime.session_context.lock().await.clone();
            app.todos = runtime.todos.lock().await.clone();
            app.turn_count = *runtime.turn_count.lock().await;
            app.current_round = *runtime.current_round.lock().await;
            app.review_alert = runtime.review_alert.lock().await.clone();
            app.turn_started_at = *runtime.turn_started_at.lock().await;
            app.pending_permission = runtime.pending_permission.lock().await.front().cloned();
            app.key_status = runtime.key_status.lock().await.clone();
            app.provider_picker = runtime.provider_picker.lock().await.clone();
            if app.pending_permission.is_some() && app.active_modal == Modal::None {
                app.active_modal = Modal::Permission;
                app.modal_index = 0;
                app.permission_scroll = 0;
                app.permission_show_details = false;
                // A permission prompt is urgent: clear any focused transcript
                // step so the next keypress decides the sheet, not the step.
                app.focused_target = None;
            } else if app.pending_permission.is_none() && app.active_modal == Modal::Permission {
                app.active_modal = Modal::None;
                app.modal_index = 0;
                app.permission_confirm_always = false;
                app.permission_scroll = 0;
                app.permission_max_scroll = 0;
                app.permission_show_details = false;
            }
            // Question modal: mirror the pending-request queue front into the
            // App-level model. A new front (arriving request) opens a fresh
            // QuestionModel with default selections; an emptied front (after a
            // submit/cancel drained the queue) clears the model and closes the
            // modal. The model is the single source of truth for the modal's
            // interaction state once open.
            {
                let front = runtime.pending_question.lock().await.front().cloned();
                let model_matches_front = match (&app.question, &front) {
                    (Some(m), Some(req)) => m.request().id == req.id,
                    (None, None) => true,
                    _ => false,
                };
                if !model_matches_front {
                    if let Some(req) = front {
                        app.question = Some(crate::tui::question_model::QuestionModel::open(req));
                        app.question_scroll = 0;
                        app.question_modal_follow = true;
                        app.active_modal = Modal::Question;
                        app.modal_index = 0;
                        app.focused_target = None;
                    } else {
                        app.question = None;
                        if app.active_modal == Modal::Question {
                            app.active_modal = Modal::None;
                            app.modal_index = 0;
                        }
                    }
                }
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
            show_local_toast(
                app,
                format!(
                    "{n} image{} attached — enter to send",
                    if n == 1 { "" } else { "s" }
                ),
                false,
                std::time::Duration::from_millis(600),
            );
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

        // The breathing indicator's phase is derived from wall-clock time at
        // the draw site (see `spinner_epoch`), not advanced per frame: the loop
        // wakes at irregular intervals (mouse-move/hover floods, streaming,
        // paste), so a per-frame counter would make the breathing speed up and
        // stutter with input activity instead of holding a steady cadence.

        // Draw frame
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            app.modal_hit_map.clear();
            let activity_for_display = app.activity_status.as_str();
            let status = display_status(
                &app.loop_status,
                activity_for_display,
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
                let idx = tasks.iter().position(|message| {
                    message.tool_step_call_id() == Some(current.call_id.as_str())
                })?;
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
                    cell_selection: app.drag.cell_info.as_ref(),
                    activity: &status,
                    // ~100ms per phase keeps one breathing cycle near 1.2s
                    // (SPINNER_PHASES steps); `breathing_color` wraps modulo.
                    spinner_phase: (app.spinner_epoch.elapsed().as_millis() / 100) as usize,
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
                    logo: app.logo.as_deref(),
                    theme: &app.theme,
                },
            );
            let input_rect = transcript_render.input_rect;
            let hint_rect = transcript_render.hint_rect;
            let activity_rect = transcript_render.activity_rect;
            let todos_rect = transcript_render.todos_rect;
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
                        shell_active: app.focused_target.is_none()
                            && app.active_modal == Modal::None
                            && app.input.starts_with('!'),
                        unattended: app.unattended,
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
                        let permission_rect = neenee_tui::Rect::new(
                            input_rect.x,
                            input_rect.y,
                            input_rect.width,
                            input_rect.height + hint_rect.height,
                        );
                        let max_scroll = render::draw_permission_sheet(
                            f,
                            &mut app.modal_hit_map,
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
                } else if matches!(app.active_modal, Modal::Provider | Modal::ModelEditor) {
                    // These modals borrow the input line as their own field
                    // (filter / key+model), so the composer underneath would
                    // only duplicate the same `app.input` the modal already
                    // shows. Its rect stays mounted (so the footer layout is
                    // stable) but is left as recessed surface — the dim pass
                    // darkens it like the rest of the background. For the
                    // editor's key field the composer would also panic: the
                    // masked key's byte cursor is computed against the
                    // unmasked string.
                } else if !app.in_subagent_view() {
                    // The composer stays mounted for the dim-recess modals
                    // (Help / ToolStepDetail / Session /
                    // Activity) so the footer layout doesn't shift when the
                    // overlay opens or closes; the recess pass darkens it in
                    // place with the rest of the surface. When a transcript
                    // step carries keyboard focus (Ctrl+↑/↓), the composer drops
                    // to its dim "blurred" palette and hides the caret so the
                    // user can see at a glance that the next keypress targets
                    // the step, not the input box. Typing into the box clears
                    // the focus and re-brightens it immediately.
                    let step_focused = app.focused_target.is_some();
                    let show_caret = !step_focused
                        && app.active_modal == Modal::None
                        && !app.selection.is_active();
                    render::draw_composer(
                        f,
                        input_rect,
                        &app.input,
                        app.byte_cursor(),
                        !step_focused,
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
            app.todos_rect = todos_rect;
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
            let drawn_modal_rect = match app.active_modal {
                Modal::Provider => {
                    let ranked = app.models_filtered();
                    Some(render::draw_models_modal(
                        f,
                        &mut layout_map,
                        &ranked,
                        &app.current_provider,
                        &app.current_model,
                        app.modal_index,
                        &app.key_status,
                        &app.input,
                        app.cursor_position,
                        &mut app.model_scroll,
                        app.model_modal_follow,
                        app.model_search,
                        &app.theme,
                    ))
                }
                Modal::HistorySearch => {
                    let ranked = app.history_rows();
                    Some(render::draw_history_modal(
                        f,
                        &mut layout_map,
                        &app.input_history,
                        &app.input,
                        app.cursor_position,
                        &ranked,
                        app.modal_index,
                        &mut app.history_scroll,
                        app.history_modal_follow,
                        app.history_preview,
                        app.history_search,
                        &app.theme,
                    ))
                }
                Modal::Permission => None,
                Modal::Question => {
                    if let Some(ref qmodel) = app.question {
                        Some(render::draw_question_modal(
                            f,
                            &mut app.modal_hit_map,
                            qmodel.request(),
                            qmodel.current(),
                            qmodel.selected(),
                            qmodel.other_text(),
                            qmodel.highlight(),
                            &mut app.question_scroll,
                            app.question_modal_follow,
                            &app.theme,
                        ))
                    } else {
                        None
                    }
                }
                Modal::ModelEditor => {
                    let title = app
                        .editor_target
                        .and_then(|idx| PROVIDERS.get(idx))
                        .map(|s| s.name)
                        .unwrap_or("model");
                    Some(render::draw_model_editor(
                        f,
                        title,
                        app.editor_field,
                        &app.editor_key,
                        &app.editor_model,
                        &app.input,
                        app.cursor_position,
                        &app.theme,
                    ))
                }
                Modal::Help => Some(render::draw_help_modal(f, &mut app.help_scroll, &app.theme)),
                Modal::ToolStepDetail => {
                    if let Some(msg) = app
                        .tool_detail_message_idx
                        .and_then(|idx| app.messages.get(idx))
                    {
                        Some(render::draw_tool_step_detail_overlay(
                            f,
                            msg,
                            app.tool_detail_scroll,
                            &app.theme,
                        ))
                    } else {
                        None
                    }
                }
                Modal::Sessions => Some(render::draw_sessions_modal(
                    f,
                    &app.sessions_overview,
                    app.modal_index
                        .min(app.sessions_overview.len().saturating_sub(1)),
                    &app.theme,
                )),
                Modal::Session => Some(render::draw_session_modal(
                    f,
                    &app.current_provider,
                    &app.current_model,
                    &app.key_status,
                    &app.mcp_statuses,
                    app.session_context.as_ref(),
                    app.modal_index,
                    &mut app.session_scroll,
                    app.session_modal_follow,
                    &app.theme,
                )),
                Modal::Permissions => Some(render::draw_permissions_manager(
                    f,
                    app.session_context.as_ref(),
                    app.modal_index,
                    &mut app.permissions_scroll,
                    &app.theme,
                )),
                Modal::Activity => {
                    let user_prompt: Option<String> = app
                        .focused_messages()
                        .iter()
                        .rev()
                        // Only a genuine chat prompt is the turn's driving
                        // prompt. Slash commands (`/review …`) and shell
                        // passthroughs (`!ls`) are surfaced as `Role::User`
                        // in the transcript but are handled by the harness /
                        // bash tool, never seen by the model — so they must
                        // not be shown as the Activity modal's "Prompt".
                        .find(|m| {
                            m.role == neenee_core::Role::User
                                && m.origin == crate::tui::document::UserMessageOrigin::Chat
                        })
                        .map(|m| m.raw.clone());
                    Some(render::draw_activity_modal(
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
                    ))
                }
                Modal::None => None,
            };

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

            // Record the open modal's actual panel rect (when one is
            // dismissable) so a click on the backdrop outside it can close it.
            // The rect comes from the renderer that just painted the panel, so
            // dynamic-height modals and click hit-tests cannot drift apart.
            app.modal_rect = if app.active_modal.dismissable_by_outside_click() {
                drawn_modal_rect
            } else {
                None
            };
        })?;

        // Cursor visibility follows the focus zone so the caret only shows up
        // where keys actually land. While a modal is open the modal itself
        // owns the caret (and may hide it for non-edit modals like Help). The
        // input box always owns typing — a focused transcript step does not
        // blur it — so the caret stays visible while navigating steps. Active
        // selections hide the terminal cursor; otherwise a block cursor can
        // draw over CJK selection backgrounds and look like a duplicated input
        // glyph.
        // Toggled only when the desired state changes to avoid spamming the
        // terminal with redundant escape codes every frame.
        let cursor_should_hide = app.active_modal == Modal::None && app.selection.is_active();
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
        loop {
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
            // The Ctrl+R history modal's search sub-layer borrows the input line
            // as its fuzzy query, so a literal `/foo` query must NOT trigger the
            // slash completion popup (or `@path` mentions); browse mode keeps the
            // line empty. Either way, suppress completions while the modal is
            // open. The same suppression applies right after an Enter-driven
            // commit: the user just finished a completion, so the popup should
            // stay hidden until the next edit.
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
                    has_queued: !app.pending_dispatch.is_empty(),
                    history_searching: app.history_search,
                    model_searching: app.model_search,
                },
                &mut app.drag,
            );

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
                    } else if let Some((start, end)) = app.selection.active_normalized_range() {
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
                        // A slash command is surfaced as a user turn in the
                        // transcript (so history recall shows the `/cmd`), but
                        // it is NOT the prompt driving the model — the harness
                        // handles it directly. Tag it so the Activity modal
                        // does not mistake it for the turn's prompt.
                        .push(
                            TranscriptMessage::new(Role::User, cmd.clone())
                                .with_origin(UserMessageOrigin::Slash),
                        );
                    if app.input_history.last() != Some(&cmd) {
                        app.input_history.push(cmd.clone());
                    }
                    app.history_index = None;
                    // `/serve` is a pure frontend concern (hot-attach a
                    // WebSocket listener to the running session). Intercept
                    // it here rather than routing through agent_loop.
                    if cmd == "/serve" || cmd.starts_with("/serve ") {
                        let port: u16 = cmd
                            .split_whitespace()
                            .nth(1)
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(0);
                        let mut tap = app.serve_tap.lock().await;
                        if tap.is_some() {
                            // `/serve` with no arg while active = stop.
                            if cmd == "/serve" {
                                *tap = None;
                                if let Some(ct) = app.serve_cancel.take() {
                                    ct.cancel();
                                }
                                runtime.messages.lock().await.push(
                                    TranscriptMessage::new(
                                        Role::Assistant,
                                        "Serve mode stopped.".to_string(),
                                    )
                                    .with_origin(UserMessageOrigin::Slash),
                                );
                            } else {
                                runtime.messages.lock().await.push(
                                    TranscriptMessage::new(
                                        Role::Assistant,
                                        "Serve already active. Use /serve (no port) to stop."
                                            .to_string(),
                                    )
                                    .with_origin(UserMessageOrigin::Slash),
                                );
                            }
                        } else {
                            let (bc_tx, _) = broadcast::channel::<AgentResponse>(1024);
                            *tap = Some(bc_tx.clone());
                            let (port_rx, cancel_token) = neenee_server::serve::start_server(
                                port,
                                app.tx.clone(),
                                bc_tx,
                                session.clone(),
                            );
                            // Stash the cancel token so `/serve` (stop) can
                            // shut the listener down.
                            app.serve_cancel = Some(cancel_token);
                            // Wait for the listener to report the actual bound
                            // port (resolves port=0 to the OS-assigned value).
                            let actual_port = port_rx.await.unwrap_or(port);
                            let msg = format!(
                                "Serve mode started on port {}. \
                                 Open ws://localhost:{} in a WebSocket client.",
                                actual_port, actual_port,
                            );
                            runtime.messages.lock().await.push(
                                TranscriptMessage::new(Role::Assistant, msg)
                                    .with_origin(UserMessageOrigin::Slash),
                            );
                        }
                        runtime.is_responding.store(false, Ordering::SeqCst);
                        return Ok(());
                    }
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
                        // A `!command` shell passthrough runs directly through
                        // the bash tool, bypassing the model entirely — it is
                        // not the turn's driving prompt. Tag it so the
                        // Activity modal does not mistake it for one.
                        .push(
                            TranscriptMessage::new(Role::User, display.clone())
                                .with_origin(UserMessageOrigin::Shell),
                        );
                    if app.input_history.last() != Some(&display) {
                        app.input_history.push(display);
                    }
                    app.history_index = None;
                    let _ = app.tx.send(AgentRequest::ShellCommand { command });
                }
                input::InputAction::ProviderPickerActivate => {
                    if app.active_modal == Modal::Provider {
                        // Activate the highlighted row of the flat model list
                        // (falling back to the first row). Each row already pins
                        // an exact (provider, model) pair — multi-model providers
                        // are fanned out into one row per model — so there is no
                        // second-stage picker: Enter switches straight to that
                        // model. The cursor starts on the current model when the
                        // picker opens (see `OpenProvider`), so "open + Enter"
                        // re-activates the current model.
                        let ranked = app.models_filtered();
                        if let Some(row) = ranked.get(app.modal_index).or_else(|| ranked.first()) {
                            let solution = PROVIDERS[row.provider_idx];
                            let model = row.model;
                            if app.key_status.get(solution.id).copied().unwrap_or(true) {
                                // Key present (or unknown → assume usable): switch
                                // to the chosen model. The backend's SwitchProvider
                                // routes it through build_provider_for_model so the
                                // per-model transport (OpenAI vs Anthropic
                                // /messages) is selected correctly.
                                let _ = app.tx.send(AgentRequest::SwitchProvider {
                                    provider_type: solution.id.to_string(),
                                    model: model.to_string(),
                                    api_key: None,
                                    base_url: None,
                                });
                                app.restore_model_draft();
                                app.active_modal = Modal::None;
                            } else {
                                // No key configured: open the unified editor,
                                // prefilled with this exact model id, so the user
                                // can enter a key before activating. The picker
                                // filter in `input` is discarded (transient
                                // search); the chat draft stays in stashed_input.
                                app.editor_target = Some(row.provider_idx);
                                app.editor_field = 0;
                                app.editor_key.clear();
                                app.editor_model = model.to_string();
                                app.input.clear();
                                app.cursor_position = 0;
                                app.model_search = false;
                                app.active_modal = Modal::ModelEditor;
                            }
                        }
                    }
                }
                input::InputAction::ModelEnterSearch => {
                    // `/` in browse mode: enter the search sub-layer. The input
                    // line is already empty (held in `stashed_input`); typing now
                    // builds the fuzzy query and re-ranks `models_filtered`.
                    if app.active_modal == Modal::Provider {
                        app.model_search = true;
                        app.modal_index = 0;
                        app.model_scroll = 0;
                        app.model_modal_follow = true;
                    }
                }
                input::InputAction::ModelExitSearch => {
                    // First Esc while searching: drop the query and return to the
                    // full browse list. The chat draft stays parked in
                    // `stashed_input` until the modal closes for real.
                    if app.active_modal == Modal::Provider {
                        app.model_search = false;
                        app.input.clear();
                        app.cursor_position = 0;
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        app.modal_index = 0;
                        app.model_scroll = 0;
                        app.model_modal_follow = true;
                    }
                }
                input::InputAction::ProviderPickerToggleFavorite => {
                    if app.active_modal == Modal::Provider {
                        // Toggle the favorite on the highlighted row's provider
                        // (falling back to the first visible row). Sending the
                        // request is enough; the backend pushes a fresh snapshot
                        // that flips the ★ next frame.
                        let ranked = app.models_filtered();
                        if let Some(row) = ranked.get(app.modal_index).or_else(|| ranked.first()) {
                            let id = PROVIDERS[row.provider_idx].id.to_string();
                            let _ = app.tx.send(AgentRequest::ToggleFavorite { id });
                        }
                    }
                }
                input::InputAction::OpenModelEditor => {
                    // `e` in browse mode: open the unified editor for the
                    // highlighted row's provider, prefilled with that row's model
                    // id. The picker filter is discarded (transient search); the
                    // chat draft stays stashed.
                    if app.active_modal == Modal::Provider {
                        let ranked = app.models_filtered();
                        if let Some(row) = ranked.get(app.modal_index).or_else(|| ranked.first()) {
                            app.editor_target = Some(row.provider_idx);
                            app.editor_field = 0;
                            app.editor_key.clear();
                            app.editor_model = row.model.to_string();
                            app.input.clear();
                            app.cursor_position = 0;
                            app.model_search = false;
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
                    // Stash whatever the user was composing so Esc restores it
                    // unchanged. The picker opens in browse mode, so the input
                    // line stays empty until `/` enters search and borrows it as
                    // the fuzzy query (same pattern as the history modal).
                    app.stashed_input = std::mem::take(&mut app.input);
                    app.cursor_position = 0;
                    app.input_scroll = 0;
                    app.active_modal = Modal::Provider;
                    app.model_search = false;
                    app.model_scroll = 0;
                    app.model_modal_follow = true;
                    // Land the cursor on the currently-active model so the "open
                    // picker + Enter" fast path re-activates it. Activation always
                    // honors the highlighted row (see `ProviderPickerActivate`).
                    let ranked = app.models_filtered();
                    app.modal_index = ranked
                        .iter()
                        .position(|row| {
                            PROVIDERS[row.provider_idx].id == app.current_provider
                                && row.model == app.current_model
                        })
                        .or_else(|| {
                            ranked.iter().position(|row| {
                                PROVIDERS[row.provider_idx].id == app.provider_picker.default_id
                            })
                        })
                        .unwrap_or(0);
                    app.suggestion_index = None;
                }
                input::InputAction::OpenHistory => {
                    // Stash whatever the user was composing so Esc restores it
                    // unchanged. The modal opens in browse mode, so the input
                    // line stays empty until `/` enters search and borrows it as
                    // the fuzzy query.
                    app.stashed_input = std::mem::take(&mut app.input);
                    app.cursor_position = 0;
                    app.input_scroll = 0;
                    app.suggestion_index = None;
                    app.active_modal = Modal::HistorySearch;
                    app.history_search = false;
                    // Browse rows are newest-first, so index 0 is the most-recent
                    // entry — focus the top so an immediate Enter re-inserts it.
                    app.modal_index = 0;
                    app.history_scroll = 0;
                    app.history_modal_follow = true;
                    app.history_preview = false;
                }
                input::InputAction::HistoryEnterSearch => {
                    // `/` in browse mode: enter the search sub-layer. The input
                    // line is already empty (held in `stashed_input`); typing now
                    // builds the fuzzy query and re-ranks `history_rows`.
                    app.history_search = true;
                    app.modal_index = 0;
                    app.history_scroll = 0;
                    app.history_modal_follow = true;
                    app.history_preview = false;
                }
                input::InputAction::HistoryExitSearch => {
                    // First Esc while searching: drop the query and return to the
                    // full browse list. The original draft stays parked in
                    // `stashed_input` until the modal closes for real.
                    app.history_search = false;
                    app.input.clear();
                    app.cursor_position = 0;
                    app.input_scroll = 0;
                    app.suggestion_index = None;
                    app.modal_index = 0;
                    app.history_scroll = 0;
                    app.history_modal_follow = true;
                    app.history_preview = false;
                }
                input::InputAction::HistoryInsert => {
                    // Enter inside the Ctrl+R modal: pull the focused entry out
                    // of `history_rows` (the browse list or the search matches)
                    // and drop it into the input box for further editing /
                    // sending. The message is not shipped here — the user hits
                    // Enter again to send.
                    let ranked = app.history_rows();
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
                    // A programmatic input replacement — latch the dismissal so
                    // a slash-command selection doesn't flash its completion
                    // popup until the next real edit.
                    app.completion_dismissed = true;
                    app.modal_index = 0;
                    app.active_modal = Modal::None;
                }
                input::InputAction::HistoryTogglePreview => {
                    // Tab inside the Ctrl+R modal: flip between the fuzzy list
                    // and a full-text view of the selected entry. Reusing
                    // `history_scroll` as the per-entry scroll means entering
                    // preview or moving to another entry starts from the top.
                    app.history_preview = !app.history_preview;
                    app.history_scroll = 0;
                    app.history_modal_follow = true;
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
                    app.help_scroll = 0;
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
                    app.modal_index = 0;
                    app.session_scroll = 0;
                    app.session_modal_follow = true;
                    let _ = app.tx.send(AgentRequest::QuerySessionContext);
                }
                input::InputAction::OpenPermissions => {
                    // The permissions manager modal. Reached via the
                    // `/permissions` slash command (intercepted locally, never
                    // sent to the backend). Kick off a snapshot request so the
                    // rule list populates; `/permissions clear` still goes to
                    // the backend via SendSlash.
                    app.active_modal = Modal::Permissions;
                    app.modal_index = 0;
                    app.permissions_scroll = 0;
                    let _ = app.tx.send(AgentRequest::QuerySessionContext);
                }
                input::InputAction::PermissionsActivate => {
                    // Revoke the selected "always allow" rule. The harness
                    // replies with a fresh snapshot so the list re-renders.
                    if let Some(snapshot) = app.session_context.as_ref() {
                        if let Some(rule) = snapshot.permissions.get(app.modal_index) {
                            let _ = app.tx.send(AgentRequest::RevokePermission {
                                tool: rule.tool.clone(),
                                scope: rule.scope.clone(),
                            });
                        }
                    }
                }
                input::InputAction::PermissionsClearAll => {
                    // Clear every cached rule. The harness replies with a fresh
                    // (empty) snapshot.
                    let _ = app.tx.send(AgentRequest::ClearAllPermissions);
                    app.modal_index = 0;
                }
                input::InputAction::SessionSelect { forward } => {
                    // Move the tool-selection cursor (the body scroll follows
                    // it). When there are no tools yet (still loading / none),
                    // Up/Down scrolls the dashboard body directly so the other
                    // sections stay reachable.
                    let list_len = app.session_tools_len();
                    if list_len > 0 {
                        app.modal_index = if forward {
                            (app.modal_index + 1) % list_len
                        } else if app.modal_index == 0 {
                            list_len - 1
                        } else {
                            app.modal_index - 1
                        };
                        app.session_modal_follow = true;
                    } else {
                        app.session_scroll = if forward {
                            app.session_scroll.saturating_add(1)
                        } else {
                            app.session_scroll.saturating_sub(1)
                        };
                    }
                }
                input::InputAction::SessionActivate => {
                    // Toggle the selected tool. The request is sent through the
                    // normal agent channel; the harness replies with a fresh
                    // snapshot that re-renders the dashboard.
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
                    // Most modals close straight to chat. The model editor
                    // instead steps back to the model picker, so a key entry is
                    // recoverable with Esc.
                    let mut return_to_picker = false;
                    if app.active_modal == Modal::HistorySearch {
                        // Closing from either browse or search: hand the parked
                        // draft back so Esc is a true cancel, and clear the
                        // search sub-layer / preview flags for the next open.
                        app.restore_history_draft();
                    } else if app.active_modal == Modal::Provider {
                        // The input box may have been borrowed as the fuzzy
                        // filter (search sub-layer); hand the parked draft back
                        // and clear the search/scroll flags so Esc cancels
                        // cleanly. (The two-stage Esc inside search is handled
                        // earlier by `ModelExitSearch`; this path is the
                        // browse-mode close.)
                        app.restore_model_draft();
                    } else if app.active_modal == Modal::ModelEditor {
                        // Cancel the editor: discard its fields and return to
                        // the picker in browse mode. The original chat draft
                        // stays in stashed_input for when the picker itself
                        // closes.
                        app.editor_target = None;
                        app.input.clear();
                        app.cursor_position = 0;
                        app.model_search = false;
                        app.model_modal_follow = true;
                        return_to_picker = true;
                    }
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_message_idx = None;
                        app.tool_detail_scroll = 0;
                    }
                    app.active_modal = if return_to_picker {
                        Modal::Provider
                    } else {
                        Modal::None
                    };
                }
                input::InputAction::ScrollUp => {
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_scroll = app.tool_detail_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = app.activity_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Help {
                        app.help_scroll = app.help_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Session {
                        app.session_modal_follow = false;
                        app.session_scroll = app.session_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Permissions {
                        app.permissions_scroll = app.permissions_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::HistorySearch {
                        app.history_modal_follow = false;
                        app.history_scroll = app.history_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Provider {
                        app.model_modal_follow = false;
                        app.model_scroll = app.model_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Question {
                        app.question_modal_follow = false;
                        app.question_scroll = app.question_scroll.saturating_sub(1);
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
                    } else if app.active_modal == Modal::Help {
                        app.help_scroll = app.help_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::Session {
                        app.session_modal_follow = false;
                        app.session_scroll = app.session_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::Permissions {
                        app.permissions_scroll = app.permissions_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::HistorySearch {
                        app.history_modal_follow = false;
                        app.history_scroll = app.history_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::Provider {
                        app.model_modal_follow = false;
                        app.model_scroll = app.model_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::Question {
                        app.question_modal_follow = false;
                        app.question_scroll = app.question_scroll.saturating_add(1);
                    } else {
                        app.pin_summary_line = None;
                        app.scroll = app.scroll.saturating_add(4).min(app.max_scroll);
                        if app.scroll >= app.max_scroll {
                            app.follow_bottom = true;
                        }
                    }
                }
                input::InputAction::ScrollPageUp => {
                    let step = app.view_height.saturating_sub(1).max(1);
                    if app.active_modal == Modal::Session {
                        app.session_modal_follow = false;
                        app.session_scroll = app.session_scroll.saturating_sub(step as usize);
                    } else if app.active_modal == Modal::Permissions {
                        app.permissions_scroll =
                            app.permissions_scroll.saturating_sub(step as usize);
                    } else if app.active_modal == Modal::HistorySearch {
                        app.history_modal_follow = false;
                        app.history_scroll = app.history_scroll.saturating_sub(step as usize);
                    } else if app.active_modal == Modal::Provider {
                        app.model_modal_follow = false;
                        app.model_scroll = app.model_scroll.saturating_sub(step as usize);
                    } else if app.active_modal == Modal::Help {
                        app.help_scroll = app.help_scroll.saturating_sub(step as usize);
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = app.activity_scroll.saturating_sub(step as usize);
                    } else if app.active_modal == Modal::Question {
                        app.question_modal_follow = false;
                        app.question_scroll = app.question_scroll.saturating_sub(step as usize);
                    } else {
                        app.follow_bottom = false;
                        app.pin_summary_line = None;
                        app.scroll = app.scroll.saturating_sub(step);
                    }
                }
                input::InputAction::ScrollPageDown => {
                    let step = app.view_height.saturating_sub(1).max(1);
                    if app.active_modal == Modal::Session {
                        app.session_modal_follow = false;
                        app.session_scroll = app.session_scroll.saturating_add(step as usize);
                    } else if app.active_modal == Modal::Permissions {
                        app.permissions_scroll =
                            app.permissions_scroll.saturating_add(step as usize);
                    } else if app.active_modal == Modal::HistorySearch {
                        app.history_modal_follow = false;
                        app.history_scroll = app.history_scroll.saturating_add(step as usize);
                    } else if app.active_modal == Modal::Provider {
                        app.model_modal_follow = false;
                        app.model_scroll = app.model_scroll.saturating_add(step as usize);
                    } else if app.active_modal == Modal::Help {
                        app.help_scroll = app.help_scroll.saturating_add(step as usize);
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = app.activity_scroll.saturating_add(step as usize);
                    } else if app.active_modal == Modal::Question {
                        app.question_modal_follow = false;
                        app.question_scroll = app.question_scroll.saturating_add(step as usize);
                    } else {
                        app.pin_summary_line = None;
                        app.scroll = app.scroll.saturating_add(step).min(app.max_scroll);
                        if app.scroll >= app.max_scroll {
                            app.follow_bottom = true;
                        }
                    }
                }
                input::InputAction::ScrollTop => {
                    if app.active_modal == Modal::Session {
                        app.session_modal_follow = false;
                        app.session_scroll = 0;
                    } else if app.active_modal == Modal::Permissions {
                        app.permissions_scroll = 0;
                    } else if app.active_modal == Modal::HistorySearch {
                        app.history_modal_follow = false;
                        app.history_scroll = 0;
                    } else if app.active_modal == Modal::Provider {
                        app.model_modal_follow = false;
                        app.model_scroll = 0;
                    } else if app.active_modal == Modal::Help {
                        app.help_scroll = 0;
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = 0;
                    } else if app.active_modal == Modal::Question {
                        app.question_modal_follow = false;
                        app.question_scroll = 0;
                    } else {
                        app.follow_bottom = false;
                        app.pin_summary_line = None;
                        app.scroll = 0;
                    }
                }
                input::InputAction::ScrollBottom => {
                    // Modal scroll bounds are clamped by render_body each
                    // frame, so a large number here just means "go to end".
                    if app.active_modal == Modal::Session {
                        app.session_modal_follow = false;
                        app.session_scroll = usize::MAX;
                    } else if app.active_modal == Modal::Permissions {
                        app.permissions_scroll = usize::MAX;
                    } else if app.active_modal == Modal::HistorySearch {
                        app.history_modal_follow = false;
                        app.history_scroll = usize::MAX;
                    } else if app.active_modal == Modal::Provider {
                        app.model_modal_follow = false;
                        app.model_scroll = usize::MAX;
                    } else if app.active_modal == Modal::Help {
                        app.help_scroll = usize::MAX;
                    } else if app.active_modal == Modal::Activity {
                        app.activity_scroll = usize::MAX;
                    } else if app.active_modal == Modal::Question {
                        app.question_modal_follow = false;
                        app.question_scroll = usize::MAX;
                    } else {
                        app.pin_summary_line = None;
                        app.scroll = app.max_scroll;
                        app.follow_bottom = true;
                    }
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
                        app.drag.cell_info.as_ref(),
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
                        app.drag.cell_info.as_ref(),
                    ) {
                        clipboard_ops::spawn_clipboard_copy(&copy_tx, copy_pending.clone(), text);
                    } else if app.active_modal == Modal::HistorySearch {
                        // Cancel the history modal: restore the in-progress draft
                        // the user was composing before Ctrl+R (clears the search
                        // query and sub-flags too).
                        app.restore_history_draft();
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
                    } else if !app.input.is_empty() {
                        // Ctrl+C is purely a compose-level action: copy,
                        // close overlay, clear, or quit. It never interrupts a
                        // running turn — only double-Esc does — so a task in
                        // flight is left untouched here and the input is
                        // cleared instead. Clearing the input also arms the
                        // quit window so
                        // the chain is exactly two presses total (clear,
                        // then quit). The combined toast says both what
                        // just happened and what the next Ctrl+C will do,
                        // removing the old "silent clear → user can't tell
                        // if the next press will quit or do something else"
                        // ambiguity. Pending-image reminders skip their
                        // per-frame refresh while the quit window is armed
                        // so this toast keeps the floor.
                        app.input.clear();
                        app.cursor_position = 0;
                        app.input_scroll = 0;
                        show_local_toast(
                            app,
                            "input cleared — Ctrl+C again to exit",
                            false,
                            std::time::Duration::from_millis(2000),
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
                    // Ctrl+↓ (or ↓ while focused): advance to the next step.
                    // From no focus this lands on the first (oldest) step.
                    app.focus_interactive_target(1);
                }
                input::InputAction::FocusPrevTarget => {
                    // Ctrl+↑ (or ↑ while focused): step back. From no focus this
                    // lands on the last (nearest-to-prompt) step.
                    app.focus_interactive_target(-1);
                }
                input::InputAction::ClearFocusedTarget => {
                    // Esc: drop the focus highlight, returning every key to its
                    // ordinary input-box meaning.
                    app.focused_target = None;
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
                    // applied on a later frame (image -> attach, text ->
                    // insert on the main prompt, or inline splice into the
                    // focused modal field). `apply_clipboard_paste` branches
                    // on the active modal at apply time, so a paste spawned
                    // inside a modal that the user closed before the read
                    // returned lands in the main prompt rather than being
                    // dropped.
                    clipboard_ops::spawn_clipboard_paste(&paste_tx);
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
                    // Typing into the input box reclaims it as the active
                    // surface: drop any transcript-step focus so the composer
                    // re-brightens and the next arrow key resumes caret movement
                    // rather than step navigation.
                    app.focused_target = None;
                    // Reconcile attachments: if the user typed inside a chip
                    // (breaking its syntax) the backing staged entry must be
                    // dropped, and surviving chips relabeled.
                    app.reconcile_attachments();
                }
                input::InputAction::Backspace => {
                    app.suggestion_index = None;
                    app.completion_dismissed = false;
                    // Same as InsertChar: editing the input box reclaims focus
                    // from any transcript step.
                    app.focused_target = None;
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
                    // Note: slash-command accepts latch the dismissal flag
                    // inside accept_completion (terminal accept), so Tab on
                    // `/pursue` exits completion just like Enter. `@path`
                    // accepts stay live so Tab keeps cycling candidates.
                }
                input::InputAction::CommitSuggestion(idx_str) => {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        app.accept_completion(idx);
                    }
                    // Enter always "finishes" the completion regardless of
                    // kind: drop the highlight and latch the dismissal flag
                    // so the popup stays hidden until the next edit. For
                    // slash commands this mirrors what accept_completion
                    // already did; for `@path` it is Enter-specific (Tab on
                    // a path stays live so the user can keep cycling).
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
                            None => {
                                // First ↑: stash the in-progress draft so a
                                // later ↓ past the newest entry restores it
                                // instead of leaving the composer empty.
                                app.history_draft = std::mem::take(&mut app.input);
                                app.input_history.len() - 1
                            }
                        };
                        app.history_index = Some(new_idx);
                        app.input = app.input_history[new_idx].clone();
                        app.cursor_position = app.input.chars().count();
                        // History navigation is a programmatic input replacement,
                        // not an edit — so it latches `completion_dismissed` like
                        // a slash-command accept rather than re-enabling the popup
                        // the way InsertChar/Backspace do. This keeps a recalled
                        // slash command from flashing its completion menu until
                        // the next real keystroke clears the latch.
                        app.suggestion_index = None;
                        app.completion_dismissed = true;
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
                            // Same programmatic-replacement latch as HistoryPrev.
                            app.suggestion_index = None;
                            app.completion_dismissed = true;
                        } else {
                            // Walked past the newest entry: restore the draft
                            // the user was composing before the first ↑,
                            // rather than blanking the composer.
                            app.history_index = None;
                            app.input = std::mem::take(&mut app.history_draft);
                            app.cursor_position = app.input.chars().count();
                            // The restored draft may be a partial slash/path
                            // the user was mid-edit on, but it still arrived
                            // via navigation rather than a keystroke, so hold
                            // the latch until the next edit.
                            app.suggestion_index = None;
                            app.completion_dismissed = true;
                        }
                    }
                }
                input::InputAction::ModalUp => match app.active_modal {
                    Modal::Provider => {
                        // Walk the fuzzy-filtered model list, not the raw
                        // catalog, so the cursor never lands on a hidden row
                        // (same rule as the history-search modal).
                        let count = app.models_filtered().len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                        app.model_modal_follow = true;
                    }
                    Modal::HistorySearch => {
                        // Up/Down walk the fuzzy-filtered list, not the raw
                        // history, so the cursor never lands on an entry the
                        // user cannot actually see or select.
                        let count = app.history_rows().len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                        app.history_modal_follow = true;
                        // In preview mode the body shows the focused entry's
                        // full text, so moving to another entry re-anchors it
                        // to the top.
                        if app.history_preview {
                            app.history_scroll = 0;
                        }
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
                    Modal::Permissions => {
                        let count = app
                            .session_context
                            .as_ref()
                            .map(|s| s.permissions.len())
                            .unwrap_or(0);
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
                        let count = app.models_filtered().len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                        app.model_modal_follow = true;
                    }
                    Modal::HistorySearch => {
                        let count = app.history_rows().len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                        app.history_modal_follow = true;
                        if app.history_preview {
                            app.history_scroll = 0;
                        }
                    }
                    Modal::Permission => {
                        let count = if app.permission_confirm_always { 2 } else { 4 };
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Sessions => {
                        let count = app.sessions_overview.len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Permissions => {
                        let count = app
                            .session_context
                            .as_ref()
                            .map(|s| s.permissions.len())
                            .unwrap_or(0)
                            .max(1);
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
                        if let Some(qm) = app.question.take() {
                            app.question =
                                Some(qm.update(crate::tui::question_model::QuestionAction::Up).0);
                            // Moving the highlight re-enables follow so the body
                            // scrolls to keep the cursor visible.
                            app.question_modal_follow = true;
                        }
                    }
                }
                input::InputAction::QuestionDown => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            app.question = Some(
                                qm.update(crate::tui::question_model::QuestionAction::Down)
                                    .0,
                            );
                            app.question_modal_follow = true;
                        }
                    }
                }
                input::InputAction::QuestionToggle => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            app.question = Some(
                                qm.update(crate::tui::question_model::QuestionAction::Toggle)
                                    .0,
                            );
                        }
                    }
                }
                input::InputAction::QuestionSelect(n) => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            app.question = Some(
                                qm.update(crate::tui::question_model::QuestionAction::Select(n))
                                    .0,
                            );
                            // A digit jump moves the highlight, so follow it.
                            app.question_modal_follow = true;
                        }
                    }
                }
                input::InputAction::QuestionSubmit => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            let (qm, effects) =
                                qm.update(crate::tui::question_model::QuestionAction::Submit);
                            // Keep the model until the per-frame queue sync clears
                            // it; the Closed effect drives the channel reply + drain.
                            app.question = Some(qm);
                            question_effects::apply(&effects, app, &runtime).await;
                        }
                    }
                }
                input::InputAction::QuestionCancel => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            let (_qm, effects) =
                                qm.update(crate::tui::question_model::QuestionAction::Cancel);
                            // Cancel discards the model immediately; the Closed
                            // effect drives the (empty-answers) reply + drain.
                            question_effects::apply(&effects, app, &runtime).await;
                        }
                    }
                }
                input::InputAction::QuestionInsertChar(c) => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            app.question = Some(
                                qm.update(crate::tui::question_model::QuestionAction::InsertChar(
                                    c,
                                ))
                                .0,
                            );
                        }
                    }
                }
                input::InputAction::QuestionBackspace => {
                    if app.active_modal == Modal::Question {
                        if let Some(qm) = app.question.take() {
                            app.question = Some(
                                qm.update(crate::tui::question_model::QuestionAction::Backspace)
                                    .0,
                            );
                        }
                    }
                }
                input::InputAction::PermissionSubmit => {
                    handle_permission_submit(app, &runtime).await;
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
                    if app.active_modal == Modal::Question {
                        if let Some(hit) = app.modal_hit_map.question_option_at(x, y) {
                            if let Some(qm) = app.question.take() {
                                app.question = Some(
                                    qm.update(crate::tui::question_model::QuestionAction::Select(
                                        hit.option_index + 1,
                                    ))
                                    .0,
                                );
                                app.question_modal_follow = true;
                            }
                        }
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.active_modal == Modal::Permission
                        && let Some(hit) = app.modal_hit_map.permission_action_at(x, y)
                    {
                        app.modal_index = hit.action_index;
                        handle_permission_submit(app, &runtime).await;
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.active_modal == Modal::Permission
                        && app.modal_hit_map.permission_sheet_contains(x, y)
                    {
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if let Some(r) = app.modal_rect {
                        // Click-to-dismiss: while a dismissable overlay modal is
                        // open, the full-screen backdrop owns the click — a press
                        // outside the panel closes the modal (mirroring Esc), and a
                        // press inside is a no-op (these info modals have no click
                        // targets yet). Either way the click is consumed so it does
                        // not also fall through to the transcript behind the
                        // backdrop. Modals that hold precious input and need their
                        // own restore path (Provider / ModelEditor) report no rect
                        // and are skipped here, so a stray click never discards an
                        // API key. HistorySearch *is* dismissable: its filter is
                        // ephemeral and the draft is parked, so an outside click
                        // restores the draft (mirroring Esc / CloseModal).
                        let inside =
                            r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height;
                        if !inside {
                            if app.active_modal == Modal::HistorySearch {
                                app.restore_history_draft();
                            }
                            app.active_modal = Modal::None;
                        }
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.active_modal == Modal::None
                        && app.todos_rect.is_some_and(|r| {
                            // `todos d/t` badge: open the Activity modal on the
                            // Todos section directly. Checked before the full-bar
                            // rect since the badge sits inside the bar.
                            r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                        })
                    {
                        // The activity bar may still be painted while a modal
                        // owns the surface — especially the pending Permission
                        // sheet, whose expanded body grows up over this row.
                        // Gate on `Modal::None` so a click never stacks an
                        // Activity modal on top of an in-progress decision.
                        app.active_modal = Modal::Activity;
                        app.activity_tab = ActivityTab::Todos;
                        app.modal_index = 0;
                        app.activity_scroll = 0;
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.active_modal == Modal::None
                        && app.activity_rect.is_some_and(|r| {
                            r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                        })
                    {
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
                        // Clicking the sticky header focuses that step (set
                        // above), so keyboard navigation can continue from it.
                        app.selection = SelectionState::None;
                        app.drag.cancel();
                    } else {
                        // ── Unified content hit-test cascade ──
                        // interaction::classify_click runs the full priority
                        // chain (input box → step summary → table cell →
                        // generic content → gap → dead) so the event loop
                        // only needs a single match.
                        match interaction::classify_click(&app.layout_map, x, y) {
                            ClickTarget::InputBox { cursor } => {
                                // Click inside the live input box: clear any
                                // focused step so the next keypress edits rather
                                // than acting on a step.
                                app.focused_target = None;
                                app.drag.begin_range(&mut app.selection, cursor);
                            }
                            ClickTarget::StepSummary { message_idx, kind } => {
                                // Clicked a step summary: navigate into a subagent
                                // task, otherwise toggle that step's disclosure.
                                let mi = message_idx;
                                app.focused_target = Some(kind.focus_target(mi));
                                let mut messages = runtime.messages.lock().await;
                                match kind {
                                    StepKind::ToolStep => {
                                        let enter_id = resolve_focused_mut(
                                            &mut messages,
                                            &app.focus_stack,
                                            mi,
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
                                            app.toggle_step_pinned(&mut messages, mi);
                                            drop(messages);
                                        }
                                    }
                                    StepKind::Thinking => {
                                        app.toggle_step_pinned(&mut messages, mi);
                                        drop(messages);
                                    }
                                }
                                app.selection = SelectionState::None;
                                app.drag.cancel();
                            }
                            ClickTarget::TableCell {
                                message_idx,
                                block_idx,
                                cursor,
                                cell_text,
                                cell_segments,
                                ..
                            } => {
                                // A cell drag is clamped to `│` boundaries: the
                                // pointer may wander anywhere but the selection
                                // can never cross a `│` border into an adjacent
                                // cell.  Within the cell the user has free
                                // substring selection — no auto-full-select.
                                app.drag.begin_cell(
                                    &mut app.selection,
                                    cursor,
                                    CellDragInfo {
                                        message_idx,
                                        block_idx,
                                        cell_text,
                                        segments: cell_segments,
                                    },
                                );
                                app.focused_target = None;
                            }
                            ClickTarget::Content { cursor } => {
                                // A plain click does NOT select — it only arms a
                                // drag. A zero-length range is created so an
                                // immediate drag extends it normally.
                                app.drag.begin_range(&mut app.selection, cursor);
                                app.focused_target = None;
                            }
                            ClickTarget::ContentGap => {
                                // Click inside the content band but not on a
                                // region: clear any step focus and selection
                                // without starting a text selection.
                                app.selection = SelectionState::None;
                                app.focused_target = None;
                                app.drag.cancel();
                            }
                            ClickTarget::Dead => {
                                // Click outside all known areas (outer gutters,
                                // below content). Fully inert.
                                app.selection = SelectionState::None;
                                app.focused_target = None;
                                app.drag.cancel();
                            }
                        }
                    }
                }
                input::InputAction::RightClick { x, y } => {
                    // Right-click on a tool-step summary opens the full-output
                    // detail overlay. For permission-denied steps this is the
                    // fastest way to surface the "Permission denied" message
                    // and the terminated-turn feedback.
                    if let ClickTarget::StepSummary {
                        message_idx,
                        kind: StepKind::ToolStep,
                    } = interaction::classify_click(&app.layout_map, x, y)
                    {
                        app.focused_target = Some(InteractiveTarget::tool_step(message_idx));
                        app.tool_detail_message_idx = Some(message_idx);
                        app.tool_detail_scroll = 0;
                        app.active_modal = Modal::ToolStepDetail;
                    }
                    app.selection = SelectionState::None;
                    app.drag.cancel();
                }
                input::InputAction::SelectionUpdate { x, y } => {
                    app.drag
                        .update_from_point(&mut app.selection, &app.layout_map, x, y);
                }
                input::InputAction::SelectionEnd => {
                    app.drag.finish(&mut app.selection);
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
                    } else {
                        app.hovered_step = match interaction::classify_click(&app.layout_map, x, y)
                        {
                            ClickTarget::StepSummary { message_idx, .. } => Some(message_idx),
                            _ => None,
                        };
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
    focus_stack: &[crate::tui::app::ZoomFrame],
    mi: usize,
) -> Option<&'a mut TranscriptMessage> {
    let Some(current) = focus_stack.last() else {
        return messages.get_mut(mi);
    };
    let task_idx = messages.iter().position(|message| {
        message.is_subagent_task() && message.tool_step_call_id() == Some(current.call_id.as_str())
    })?;
    messages[task_idx].subagent_children_mut()?.get_mut(mi)
}

/// Iterate mutable messages in the currently focused view (the root
/// conversation, or the focused subagent task's child stream) for bulk
/// expand/collapse operations. Callers filter by kind as needed.
pub(super) fn focused_messages_mut<'a>(
    messages: &'a mut [TranscriptMessage],
    focus_stack: &[crate::tui::app::ZoomFrame],
) -> Box<dyn Iterator<Item = &'a mut TranscriptMessage> + 'a> {
    match focus_stack.last() {
        None => Box::new(messages.iter_mut()),
        Some(current) => {
            let task_idx = messages.iter().position(|message| {
                message.is_subagent_task()
                    && message.tool_step_call_id() == Some(current.call_id.as_str())
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
/// depending on which the semantic selection covers. `cell_info` supplies the
/// cell context when the selection is a [`Range`] bounded inside a table cell.
pub(super) fn extract_selection_text(
    sel: &SelectionState,
    messages: &[crate::tui::document::TranscriptMessage],
    input: &str,
    layout_map: &crate::tui::layout::LayoutMap,
    cell_info: Option<&CellDragInfo>,
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
        return get_selected_text(
            sel,
            messages,
            &|mi, bi| layout_map.table_grid(mi, bi),
            cell_info,
        );
    }
    match sel {
        SelectionState::Block { .. } => Some(input.to_string()),
        SelectionState::Range { .. } => {
            let (start, end) = sel.active_normalized_range()?;
            let start = floor_grapheme_boundary(input, start.byte_offset);
            let end = inclusive_grapheme_end(input, end.byte_offset);
            (start < end).then(|| input[start..end].to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod selection_text_tests {
    use super::*;
    use crate::tui::layout::{LayoutMap, SemanticCursor};
    use crate::tui::render::INPUT_MSG_IDX;
    use crate::tui::selection::SelectionState;

    #[test]
    fn input_collapsed_selection_copies_nothing() {
        let cursor = SemanticCursor::new(INPUT_MSG_IDX, 0, 0);
        let sel = SelectionState::Range {
            anchor: cursor,
            head: cursor,
        };

        assert_eq!(
            extract_selection_text(&sel, &[], "中文", &LayoutMap::new(), None),
            None
        );
    }

    #[test]
    fn input_wide_glyph_drag_copies_one_grapheme() {
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(INPUT_MSG_IDX, 0, 0),
            head: SemanticCursor::new(INPUT_MSG_IDX, 0, 1),
        };

        assert_eq!(
            extract_selection_text(&sel, &[], "中文", &LayoutMap::new(), None),
            Some("中".to_string())
        );
    }
}

fn show_local_toast(
    app: &mut App,
    message: impl Into<String>,
    failed: bool,
    duration: std::time::Duration,
) {
    app.copy_toast_message = message.into();
    app.copy_toast_failed = failed;
    app.copy_toast_until = Some(std::time::Instant::now() + duration);
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

/// Execute the side effects that the pure [`QuestionModel::update`] described.
///
/// This is the effect interpreter — the *only* place the question modal touches
/// the agent channel, the pending-request queue, or the modal/queue sync. The
/// `Reply` effect looks up the subagent parent routing key (so a subagent's
/// answer routes back down to it), sends the reply, and removes the request
/// from the queue; `Closed` does the same minus the reply (empty answers). In
/// both cases the per-frame queue sync (above) picks up the new queue front on
/// the next iteration and opens the next queued question or closes the modal.
mod question_effects {
    use super::{AgentRequest, App, Modal, UiRuntime};

    pub(super) async fn apply(
        effects: &[crate::tui::question_model::QuestionEffect],
        app: &mut App,
        runtime: &UiRuntime,
    ) {
        for effect in effects {
            match effect {
                crate::tui::question_model::QuestionEffect::Reply {
                    request_id,
                    answers,
                } => {
                    let parent_call_id = runtime
                        .subagent_question_parent
                        .lock()
                        .await
                        .remove(request_id);
                    let _ = app.tx.send(AgentRequest::UserQuestionReply {
                        request_id: request_id.clone(),
                        answers: answers.clone(),
                        parent_call_id,
                    });
                }
                // Draining the queue + settling the modal is shared by both
                // Close-causing effects (Submit → Reply+Closed, Cancel → Closed).
                // The per-frame sync re-derives the model from the queue front,
                // so here we only need to drop the answered/cancelled request
                // and clear the stale modal state.
                crate::tui::question_model::QuestionEffect::Closed { request_id } => {
                    let mut queue = runtime.pending_question.lock().await;
                    queue.retain(|r| r.id != *request_id);
                    // If the queue is now empty the modal closes; the sync block
                    // will also clear `app.question`, but clearing it here keeps
                    // the very next render (same frame) consistent.
                    if queue.is_empty() {
                        app.question = None;
                        app.active_modal = Modal::None;
                        app.modal_index = 0;
                    }
                }
            }
        }
    }
}
