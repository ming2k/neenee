//! The TUI's central application state ([`App`]) plus the [`Modal`] kind and
//! the `impl App` blocks that hold pure state-management methods.
//!
//! Input-box completion lives in [`crate::tui::completion`]; the event/render
//! loop and shared runtime live in [`crate::tui::event_loop`]. Everything
//! else that mutates `App` either lives here (state navigation, focus,
//! sticky/pinned step bookkeeping) or in `completion.rs` (the only other
//! `impl App` block).
//!
//! [`crate::tui::completion`]: crate::tui::completion

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use neenee_core::{
    mcp::McpConnectionStatus, AgentRequest, Goal, ImagePart, PermissionRequest, PlanProgress,
    ProviderPickerRow, ProviderPickerSnapshot, Role, SessionOverview, UserQuestionRequest,
};

use crate::tui::completion::PathScan;
use crate::tui::composer_attachments;
use crate::tui::config;
use crate::tui::document::{DeliveryStatus, TranscriptMessage};
use crate::tui::event_loop::resolve_focused_mut;
use crate::tui::fuzzy;
use crate::tui::input;
use crate::tui::layout::{InteractiveTarget, LayoutMap};
use crate::tui::providers::{providers_filtered_from, PROVIDERS};
use crate::tui::render::Theme;
use crate::tui::selection::{SelectionDrag, SelectionState};

use std::collections::{HashMap, VecDeque};

/// A user message staged in the send queue, waiting for the in-flight turn
/// to finish before it is dispatched to the agent.
///
/// The queue is the single source of truth for *what* to send when the
/// harness returns to idle; the matching visual marker lives on the
/// [`TranscriptMessage`] (carrying [`crate::tui::document::DeliveryStatus::Queued`]).
/// The two are kept in sync by the event loop:
///
/// - Send-while-busy pushes one of these **and** a queued transcript message.
/// - Idle dispatch pops the front of this queue **and** flips the first
///   queued transcript message to `Delivered`.
/// - Up-arrow recall pops the back of this queue **and** removes the last
///   queued transcript message.
///
/// FIFO dispatch + LIFO recall never collide because both pop directions
/// match the corresponding transcript message in transcript order.
#[derive(Debug, Clone)]
pub struct QueuedDispatch {
    /// The user's literal prompt text, sent verbatim to the agent on dispatch.
    pub text: String,
    /// Pasted images staged for this message (Ctrl+V). Empty for plain text.
    pub images: Vec<ImagePart>,
    /// Large pasted text blocks staged behind `[Pasted text #N +M lines]`
    /// chips inside `text`. Empty for plain-text drafts. Order matches the
    /// chip numbering, so the Nth chip expands to `pending_text_pastes[N-1]`.
    pub text_pastes: Vec<String>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Modal {
    None,
    Provider,
    HistorySearch,
    Permission,
    Question,
    /// Unified provider editor: edit the API key and model-id
    /// of a catalog entry in one place. Reached via `e` in the picker or
    /// `Enter` on a no-key model. Replaces the sequential ApiKey / Endpoint /
    /// ModelName modal chain.
    ModelEditor,
    Help,
    Sessions,
    /// Full-output detail overlay for a focused tool step. The step is
    /// identified by `App::tool_detail_message_idx`; `tool_detail_scroll`
    /// holds the overlay's own scroll offset.
    ToolStepDetail,
    /// Session context modal: a tabbed overview of the live session's model,
    /// MCP servers, permissions, tools, and skills. Opened with the `/session`
    /// slash command; the active pane is [`App::session_tab`].
    Session,
    /// Read-only preview of the active plan file. Opened by clicking the
    /// sticky plan panel above the input box or pressing `Ctrl+P` while a
    /// plan is active. The content is loaded from disk at open time and
    /// stored in [`App::plan_preview_content`]; `plan_preview_scroll` holds
    /// the overlay's own scroll offset.
    PlanPreview,
    /// Activity overview: the current goal (objective + checklist), the live
    /// plan-progress breakdown, and the running turn/round/model/elapsed/
    /// status. Opened by clicking the activity bar. The body scrolls via
    /// [`App::activity_scroll`].
    Activity,
}

/// Active pane inside the session-context modal. The variant order defines the
/// tab-strip order (left → right) and the Left/Right cycle order.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum SessionTab {
    Model,
    Mcp,
    Skills,
    Permissions,
    Tools,
}

impl SessionTab {
    /// All panes in tab-strip order. Used by the renderer to build the strip
    /// and by the Left/Right cycle to step through them.
    pub const ALL: [SessionTab; 5] = [
        SessionTab::Model,
        SessionTab::Mcp,
        SessionTab::Skills,
        SessionTab::Permissions,
        SessionTab::Tools,
    ];

    /// Short label shown in the tab strip.
    pub fn label(self) -> &'static str {
        match self {
            SessionTab::Model => "Model",
            SessionTab::Mcp => "MCP",
            SessionTab::Skills => "Skills",
            SessionTab::Permissions => "Permissions",
            SessionTab::Tools => "Tools",
        }
    }

    /// Step to the neighbouring pane. `forward` = right, else left. Wraps.
    pub fn cycle(self, forward: bool) -> SessionTab {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        let n = Self::ALL.len();
        let next = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        Self::ALL[next]
    }
}

pub struct App {
    pub input: String,
    /// Structured transcript messages (semantic document model).
    pub messages: Vec<TranscriptMessage>,
    pub scroll: u16,
    /// Whether the view follows the newest content (auto-scroll to bottom).
    pub follow_bottom: bool,
    /// Last measured stream height in lines and viewport height, used to pin
    /// the view to the bottom while following.
    pub content_lines: usize,
    pub view_height: u16,
    pub max_scroll: u16,
    /// Expanded step pinned under the HUD bar (its message index + screen rect),
    /// when its body is scrolled into view. Clicks inside the rect collapse it.
    pub sticky_step: Option<usize>,
    pub sticky_rect: Option<ratatui::layout::Rect>,
    /// Screen rect of the activity bar for the current frame, so clicks inside
    /// it open the Activity modal. `None` when no activity bar is shown (idle,
    /// streaming, sub-agent view, or chrome hidden).
    pub activity_rect: Option<ratatui::layout::Rect>,
    /// Content-line index of the sticky step's real summary. Used to re-anchor
    /// the scroll offset when the user collapses the pinned step so the summary
    /// lands at the top of the viewport instead of jumping to unrelated content.
    pub sticky_summary_line: Option<usize>,
    /// Content-line the user asked to keep pinned at the top of the viewport by
    /// collapsing a sticky summary. While set, the per-frame scroll clamp is
    /// allowed to scroll past the natural `max_scroll` so a short tail of
    /// content below the collapsed step does not yank the header back down.
    /// Cleared on any manual scroll, view reset, or when auto-follow resumes.
    pub pin_summary_line: Option<usize>,
    /// Stack of sub-agent task call-ids that the view is zoomed into. Empty
    /// means the root conversation is shown; a non-empty stack renders the
    /// focused `task` tool step's child messages as the main stream, with a
    /// navigation bar to return to the parent or cycle sibling sub-agents.
    pub focus_stack: Vec<String>,
    pub tx: mpsc::UnboundedSender<AgentRequest>,
    pub should_quit: Arc<AtomicBool>,
    pub suggestion_index: Option<usize>,
    /// Latched whenever the user explicitly finishes a completion with Enter
    /// (or dismissed one by sending the message). While `true`, the completion
    /// popup is suppressed even if `completion_kind()` would otherwise show
    /// one — so accepting `/mode` does not immediately flash the
    /// `/mode build` / `/mode plan` subcommand menu. Cleared by the next
    /// `InsertChar` / `Backspace` (the user is editing again, so live
    /// completions are once again useful).
    pub completion_dismissed: bool,
    pub custom_commands: Vec<(String, String)>,
    pub cursor_position: usize,
    pub input_scroll: usize,
    pub active_modal: Modal,
    pub modal_index: usize,
    /// Active pane inside the session-context modal ([`Modal::Session`]).
    /// Ignored while any other modal is open.
    pub session_tab: SessionTab,
    /// Body scroll offset of the session modal's active pane. Reset to 0 on
    /// open and tab change. Clamped (and, for list panes, auto-followed to the
    /// selection) by the renderer each frame.
    pub session_scroll: usize,
    pub current_provider: String,
    pub current_model: String,
    /// Raw current working directory captured at startup. Used to resolve
    /// `@path` mention completions against the real filesystem.
    pub cwd: std::path::PathBuf,
    /// Cached recursive project file listing for `@path` completion, populated
    /// lazily on the first `@` mention and reused afterwards. Mirrors the
    /// per-directory picker cache in opencode's TUI. Invalidated after each
    /// accepted path completion so newly-created files become visible without
    /// a restart. `None` = not scanned yet.
    pub path_scan_cache: Option<PathScan>,
    pub current_goal: Option<Goal>,
    /// Latest session-context snapshot for the session modal, or `None` before
    /// the first `QuerySessionContext` round-trip completes. Refreshed each
    /// frame from the response listener.
    pub session_context: Option<neenee_core::SessionContextSnapshot>,
    pub loop_status: String,
    pub activity_status: String,
    /// Whether write-tool permission prompts are bypassed this session
    /// (`--auto-approve` / `/auto-approve on`). Mirrored from the harness
    /// snapshot; shown as a badge in the hint bar so the elevated state is
    /// always visible.
    pub auto_approve: bool,
    /// Live plan progress snapshot, mirrored from the harness. Shown inside
    /// the Activity modal (and no longer pinned above the input box) so the
    /// footer reclaims the vertical space. `None` outside Build mode with an
    /// active plan.
    pub plan_progress: Option<PlanProgress>,
    /// Harness turn counter, mirrored each frame. Surfaced inside the
    /// Activity modal as `turn N`, and shown in the activity bar.
    pub turn_count: u64,
    /// Current tool round within the active turn (1-indexed for display:
    /// `0` means the turn has started but no model request has fired yet —
    /// e.g. the "queued" / "preparing context" phase). Mirrored each frame
    /// from the response listener; shown in the activity bar as
    /// `turn N · round M · <status>`.
    pub current_round: u64,
    /// Stall alert level (consecutive read-only rounds), or `0` when inactive.
    /// While > 0 the activity bar appends a `⚠ stalled: N — Esc to interrupt`
    /// segment. Mirrored each frame from the response listener.
    pub stall_rounds: u64,
    /// Wall-clock instant the current turn started, or `None` between turns.
    /// Drives the muted `<elapsed>` segment in the activity bar.
    pub turn_started_at: Option<std::time::Instant>,
    /// Cached content of the plan file currently shown in `Modal::PlanPreview`.
    /// Loaded from disk when the modal opens so the preview does not flicker
    /// on every redraw; cleared on close so the next open re-reads the file
    /// (the model may have updated it).
    pub plan_preview_content: String,
    /// Scroll offset inside `Modal::PlanPreview`. Reset to 0 each time the
    /// modal opens.
    pub plan_preview_scroll: u16,
    /// Scroll offset inside `Modal::Activity`. Reset to 0 each time the modal
    /// opens; clamped each frame by the modal's body renderer.
    pub activity_scroll: usize,
    pub pending_permission: Option<PermissionRequest>,
    pub pending_question: Option<UserQuestionRequest>,
    /// Selected option indices per question. Outer vec parallels
    /// `pending_question.questions`; inner vec holds selected option indices.
    pub question_selected: Vec<Vec<usize>>,
    /// Free-text input per question when the "Other" option is highlighted.
    pub question_other_text: Vec<String>,
    /// Keyboard focus within the question modal: which question is active.
    pub question_current: usize,
    /// Rows shown in the sessions picker (`/sessions` or `neenee resume`).
    pub sessions_overview: Vec<SessionOverview>,
    pub permission_confirm_always: bool,
    /// Whether the inline permission sheet is expanded to show the full
    /// description + arguments. Collapsed by default so the prompt stays
    /// brief; "Details" toggles this.
    pub permission_show_details: bool,
    pub permission_scroll: usize,
    pub permission_max_scroll: usize,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    /// Images pasted (Ctrl+V) and waiting to be sent with the next message.
    /// Each entry is paired 1-to-1 with an `[Image #N]` chip inside
    /// [`App::input`]; the chip's `#N` is `index + 1` after
    /// [`App::reconcile_attachments`] has run.
    pub pending_images: Vec<ImagePart>,
    /// Large pasted text blocks staged behind `[Pasted text #N +M lines]`
    /// chips inside [`App::input`]. Each entry is the full original paste;
    /// the matching chip in the input is just a short label so the input
    /// box stays compact. Order matches the chip numbering.
    pub pending_text_pastes: Vec<String>,
    /// FIFO of user messages staged while a turn was in flight. Each entry
    /// has a matching [`TranscriptMessage`] carrying
    /// [`crate::tui::document::DeliveryStatus::Queued`] in [`App::messages`].
    /// The event loop drains the front whenever the harness returns to idle,
    /// and the Up-arrow handler pops the back to recall the most-recent
    /// queued draft for editing. See [`QueuedDispatch`] for the sync rules.
    pub pending_dispatch: VecDeque<QueuedDispatch>,
    /// Semantic selection state.
    pub selection: SelectionState,
    /// Drag gesture state.
    pub drag: SelectionDrag,
    /// Layout map for the current frame (updated each draw).
    pub layout_map: LayoutMap,
    /// Message index of the step (tool step or reasoning trace) whose header
    /// currently rests under the mouse pointer (inline or sticky pinned), so
    /// the next draw lights it up to the intermediate hover tone as a click
    /// affordance. `None` whenever the pointer is elsewhere or an overlay
    /// modal is open.
    pub hovered_step: Option<usize>,
    /// Global tool-step density (false = Compact default, true = Comfortable:
    /// new tool steps spawn expanded). Shared with the response listener.
    pub tool_density: Arc<AtomicBool>,
    /// TUI display config (`[tui]` table of `config.toml`): per-step-kind
    /// default expand state. Shared with the response listener so live and
    /// restored steps both honor it.
    pub tui_config: Arc<config::TuiConfig>,
    /// Message index of the tool step shown in the [`Modal::ToolStepDetail`]
    /// overlay. `None` when the overlay is closed.
    pub tool_detail_message_idx: Option<usize>,
    /// Scroll offset (rows) of the [`Modal::ToolStepDetail`] overlay.
    pub tool_detail_scroll: u16,
    /// Keyboard-focused activatable target in the current frame. Mouse support
    /// is an acceleration path; this is the equivalent keyboard-first path.
    pub focused_target: Option<InteractiveTarget>,
    /// Which surface (input box vs conversation stream) currently owns
    /// keyboard focus. See [`input::FocusZone`] for the full semantics.
    /// Defaults to [`input::FocusZone::Compose`] so typing flows into the
    /// prompt box; `Ctrl+B` switches focus to the stream (Browse), and any
    /// printable key (typically `p`) returns to the prompt (Compose).
    pub focus_zone: input::FocusZone,
    /// Tracks the last cursor visibility command we sent to the terminal so
    /// we only emit `Hide` / `Show` escape codes when the desired state
    /// actually changes, avoiding per-frame flicker.
    pub cursor_hidden: bool,
    /// Show a brief "copied" toast. Held until this deadline elapses so the
    /// duration is wall-clock consistent regardless of the event-loop cadence.
    pub copy_toast_until: Option<std::time::Instant>,
    pub copy_toast_message: String,
    pub copy_toast_failed: bool,
    /// Ticks remaining in which a second Ctrl+C quits.
    pub ctrl_c_armed_ticks: u8,
    /// Ticks remaining in which a second Esc interrupts the running task.
    pub esc_armed_ticks: u8,
    /// Monotonic per-frame counter that drives the status bar spinner so the
    /// harness never looks frozen while a turn is in flight.
    pub spinner_tick: usize,
    /// Input stashed while the API-key modal borrows the input line.
    pub stashed_input: String,
    /// Target `PROVIDERS` index for the unified provider editor.
    pub editor_target: Option<usize>,
    /// Which editor field is focused: `0` = API key, `1` = model id.
    pub editor_field: u8,
    /// API-key buffer for the editor (the input line is borrowed for the
    /// focused field; this holds the key while the model-id field is focused).
    pub editor_key: String,
    /// Model-id buffer for the editor.
    pub editor_model: String,
    /// Lowercase provider name → whether a usable API key is configured.
    pub key_status: HashMap<String, bool>,
    /// Live model-picker snapshot (default id + per-model favorite / key-ready
    /// / last-used). Drives the `/provider` picker's rendering and sort order
    /// Refreshed from the response listener each frame.
    pub provider_picker: ProviderPickerSnapshot,
    /// Theme.
    pub theme: Theme,
    /// MCP server statuses loaded at startup. Mirrored into the header as a
    /// compact right-aligned summary.
    pub mcp_statuses: Vec<(String, McpConnectionStatus)>,
}

impl App {
    pub fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor_position)
            .unwrap_or(self.input.len())
    }

    /// Reconcile [`App::pending_images`] / [`App::pending_text_pastes`]
    /// against the chips that currently survive in [`App::input`], and
    /// relabel the surviving chips so their `#N` matches their new 1-based
    /// position in the truncated vectors. Cheap to run on every input
    /// mutation: it is a single linear scan over the input string.
    ///
    /// This is the prune + relabel pass that drops orphaned staged entries
    /// whenever the user deletes or edits a chip — by backspace, selection
    /// delete, or hand-typing over the chip text. Mirrors codex's
    /// `reconcile_deleted_elements` and claude-code's `parseReferences`
    /// effect, adapted to neenee's "chip text lives in the input" model.
    pub fn reconcile_attachments(&mut self) {
        let new_input = composer_attachments::reconcile(
            &self.input,
            &mut self.pending_images,
            &mut self.pending_text_pastes,
        );
        self.input = new_input;
    }

    /// Replace every `[Pasted text #N +M lines]` chip in `text` with the
    /// matching staged full paste, leaving image chips in place as
    /// positional labels for the model. Used at submit time so the agent
    /// receives the real paste contents instead of the chip label.
    pub fn expand_paste_chips(&self, text: &str) -> String {
        composer_attachments::expand_paste_chips(text, &self.pending_text_pastes)
    }

    /// Recall the most-recently-queued message: pop it off the back of the
    /// send queue (LIFO undo), remove its visual marker from the shared
    /// transcript, and load its text + any pasted images back into the
    /// composer so the user can edit and resend.
    ///
    /// `messages` is the shared transcript (the event loop's
    /// `runtime.messages`, already locked). Passed in by the caller so the
    /// lock scope stays explicit and the recall logic is unit-testable
    /// against a plain `Vec`.
    ///
    /// Returns `true` if a queued entry was actually recalled; `false` if
    /// the queue was empty (no-op).
    pub fn recall_queued(&mut self, messages: &mut Vec<TranscriptMessage>) -> bool {
        let Some(dispatch) = self.pending_dispatch.pop_back() else {
            return false;
        };
        // Drop the matching visual marker. The queue's back pairs with the
        // last transcript message still carrying `DeliveryStatus::Queued`,
        // so rposition is the correct match.
        if let Some(pos) = messages
            .iter()
            .rposition(|m| m.role == Role::User && m.delivery == DeliveryStatus::Queued)
        {
            messages.remove(pos);
        }
        self.input = dispatch.text;
        self.cursor_position = self.input.chars().count();
        if !dispatch.images.is_empty() {
            self.pending_images = dispatch.images;
        }
        if !dispatch.text_pastes.is_empty() {
            self.pending_text_pastes = dispatch.text_pastes;
        }
        // Clear the history cursor so a subsequent ↓ returns to an empty
        // input rather than to the now-stale history entry.
        self.history_index = None;
        true
    }

    /// Splice the `idx`-th live completion's label into [`App::input`] over
    /// its `[replace_start, replace_end)` byte range, landing the cursor
    /// just past the inserted text. Shared by `Tab` cycling and `Enter`
    /// commit. The caller is responsible for any post-splice state changes
    /// (e.g. latching [`App::completion_dismissed`] on the Enter path).
    pub fn accept_completion(&mut self, idx: usize) {
        let completions = self.completions();
        let Some(comp) = completions.get(idx) else {
            return;
        };
        let replace_start = comp.replace_start;
        let replace_end = comp.replace_end;
        let mut label = comp.label.clone();
        // File accept: append a trailing space so the user can keep typing
        // their message (matches opencode's splice behaviour). Directories
        // end in `/` and the popup re-triggers showing the dir's contents,
        // so no space is appended there.
        let is_dir = label.ends_with('/');
        if !is_dir {
            let needs_space = self
                .input
                .get(replace_end..)
                .and_then(|s| s.chars().next())
                .map(|c| !c.is_whitespace())
                .unwrap_or(true);
            if needs_space {
                label.push(' ');
            }
        }
        let mut new_input = String::with_capacity(self.input.len() + label.len());
        new_input.push_str(&self.input[..replace_start]);
        new_input.push_str(&label);
        let cursor_byte = replace_start + label.len();
        new_input.push_str(&self.input[replace_end..]);
        self.input = new_input;
        self.cursor_position = self.input[..cursor_byte].chars().count();
        // Drop the cached project scan so newly-created files become
        // visible on the next `@` mention without a restart.
        self.path_scan_cache = None;
    }

    pub fn cursor_display_x(&self) -> u16 {
        self.input[..self.byte_cursor()].width() as u16
    }

    /// Toggle the expansion of the tool step / reasoning trace at `mi`,
    /// keeping its header pinned to the screen position the user interacted with.
    ///
    /// A toggle inserts or removes the body lines that sit *below* the header,
    /// so the header's own content-line never moves. That gives a simple rule
    /// for keeping the header where the user clicked:
    ///
    /// - Visible (in-stream) header: leave `scroll` untouched and the header
    ///   stays on the same row; the body grows or shrinks beneath it.
    /// - Sticky-overlay header (its real header is scrolled off the top): point
    ///   `scroll` at the recorded header content-line so the real header lands
    ///   at row 0 where the overlay sat. The line is also recorded in
    ///   `pin_summary_line` so the per-frame clamp does not pull it back down
    ///   once the collapsed body shortens the stream.
    /// - Either way `follow_bottom` is cleared: the user is now pinning their
    ///   attention on this header, so the next frame's auto-follow must not
    ///   yank it away (this is what previously let an expand push the header
    ///   off-screen while the view was following the bottom).
    ///
    /// Returns `true` when a step was actually toggled, so callers can gate
    /// side effects like clearing the text selection.
    pub(crate) fn toggle_step_pinned(
        &mut self,
        messages: &mut [TranscriptMessage],
        mi: usize,
    ) -> bool {
        let pinned_to_top = self.sticky_step == Some(mi);
        let sticky_summary_line = self.sticky_summary_line;
        let toggled = resolve_focused_mut(messages, &self.focus_stack, mi)
            .map(|message| {
                if let Some(expanded) = message.tool_step_expanded() {
                    message.pin_tool_step_expanded(!expanded);
                    true
                } else if let Some(expanded) = message.thinking_expanded() {
                    message.pin_thinking_expanded(!expanded);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if toggled {
            self.follow_bottom = false;
            if pinned_to_top {
                if let Some(summary_line) = sticky_summary_line {
                    self.scroll = summary_line.min(u16::MAX as usize) as u16;
                    // Remember the line so the per-frame clamp (which runs after
                    // this, once the collapsed body has shrunk the stream) keeps
                    // allowing scroll up to it instead of yanking the summary
                    // back down to `max_scroll`.
                    self.pin_summary_line = Some(summary_line);
                }
            } else {
                // Any other toggle (e.g. expanding) is no longer pinning a
                // collapsed summary at the top: drop a stale pin so normal
                // clamping resumes.
                self.pin_summary_line = None;
            }
        }
        toggled
    }

    pub(crate) fn visible_interactive_targets(&self) -> Vec<InteractiveTarget> {
        let mut targets = self.layout_map.interactive_targets();
        if let Some(message_idx) = self.sticky_step {
            if let Some(message) = self.focused_messages().get(message_idx) {
                let target = if message.is_thinking() {
                    InteractiveTarget::thinking(message_idx)
                } else if message.is_tool_step() || message.is_subagent_task() {
                    InteractiveTarget::tool_step(message_idx)
                } else {
                    return targets;
                };
                if !targets.contains(&target) {
                    targets.insert(0, target);
                }
            }
        }
        targets
    }

    pub(crate) fn retain_visible_focused_target(&mut self) {
        if self.active_modal != Modal::None {
            self.focused_target = None;
            return;
        }
        if let Some(target) = self.focused_target {
            if !self.visible_interactive_targets().contains(&target) {
                self.focused_target = None;
            }
        }
    }

    pub(crate) fn focus_interactive_target(&mut self, direction: i8) {
        let targets = self.visible_interactive_targets();
        if targets.is_empty() {
            self.focused_target = None;
            return;
        }

        let current = self
            .focused_target
            .and_then(|target| targets.iter().position(|candidate| *candidate == target));
        let next = match (current, direction < 0) {
            (Some(0), true) => targets.len() - 1,
            (Some(idx), true) => idx - 1,
            (Some(idx), false) => (idx + 1) % targets.len(),
            (None, true) => targets.len() - 1,
            (None, false) => 0,
        };

        self.focused_target = Some(targets[next]);
        self.selection = SelectionState::None;
        self.drag.cancel();
    }

    /// Whether the view is currently zoomed into a sub-agent task.
    pub fn in_subagent_view(&self) -> bool {
        !self.focus_stack.is_empty()
    }

    /// The message slice currently in view: the root conversation, or the
    /// focused sub-agent task's child messages.
    pub fn focused_messages(&self) -> &[TranscriptMessage] {
        let Some(call_id) = self.focus_stack.last() else {
            return &self.messages;
        };
        self.messages
            .iter()
            .find_map(|message| {
                if message.is_subagent_task()
                    && message.tool_step_call_id() == Some(call_id.as_str())
                {
                    message.subagent_children()
                } else {
                    None
                }
            })
            .unwrap_or(&[])
    }

    /// Reset transient view state (scroll, selection, sticky pinning) when the
    /// focused message slice changes.
    pub(crate) fn reset_view_state(&mut self) {
        self.scroll = 0;
        self.follow_bottom = true;
        self.selection = SelectionState::None;
        self.drag.cancel();
        self.sticky_step = None;
        self.sticky_rect = None;
        self.sticky_summary_line = None;
        self.pin_summary_line = None;
        self.focused_target = None;
    }

    /// Zoom into a sub-agent task's child messages.
    pub fn enter_subagent(&mut self, call_id: String) {
        self.focus_stack.push(call_id);
        self.reset_view_state();
    }

    /// Return from the current sub-agent view to its parent. Returns true if a
    /// view was actually popped.
    pub fn exit_subagent(&mut self) -> bool {
        if self.focus_stack.pop().is_some() {
            self.reset_view_state();
            true
        } else {
            false
        }
    }

    /// Cycle to the previous (`dir < 0`) or next (`dir > 0`) sibling sub-agent
    /// task at the current focus level. No-op when not in a sub-agent view or
    /// when there are no siblings.
    pub fn cycle_sibling(&mut self, dir: i8) {
        let Some(current) = self.focus_stack.last().cloned() else {
            return;
        };
        let task_ids: Vec<String> = self
            .messages
            .iter()
            .filter_map(|message| {
                if message.is_subagent_task() {
                    message.tool_step_call_id().map(String::from)
                } else {
                    None
                }
            })
            .collect();
        let Some(idx) = task_ids.iter().position(|id| *id == current) else {
            return;
        };
        if task_ids.len() < 2 {
            return;
        }
        let n = task_ids.len() as isize;
        let next = ((idx as isize + dir as isize).rem_euclid(n)) as usize;
        self.focus_stack.pop();
        self.focus_stack.push(task_ids[next].clone());
        self.reset_view_state();
    }

    /// Fuzzy-filtered view of [`App::input_history`] for the Ctrl+R
    /// (`Modal::HistorySearch`) modal. Returns `(original_index, FuzzyMatch)`
    /// pairs sorted by descending match score, with input order as the stable
    /// tiebreaker so equally-good matches keep their top-to-bottom history
    /// order. Computed from scratch on every call: history is small and this
    /// is invoked at most a few times per frame (modal navigation, Enter
    /// accept, and rendering), so a cached field would just add stale-state
    /// risk for no measurable win.
    pub fn history_filtered(&self) -> Vec<(usize, fuzzy::FuzzyMatch)> {
        let mut ranked = fuzzy::rank(&self.input_history, &self.input);
        fuzzy::sort_by_score(&mut ranked);
        ranked
    }

    /// Compute the filtered, sorted model rows for the `/provider` picker.
    /// Delegates to [`providers_filtered_from`] so the input handler and the
    /// renderer share one filter+sort implementation.
    pub fn providers_filtered(&self) -> Vec<(usize, &ProviderPickerRow)> {
        providers_filtered_from(PROVIDERS, &self.provider_picker, self.input.trim())
    }

    /// Number of selectable rows in the session modal's active pane. Used to
    /// clamp the Up/Down row cursor. Read-only panes (Model / MCP) report 0
    /// since they have no list to navigate.
    pub fn session_tab_list_len(&self) -> usize {
        let Some(snapshot) = self.session_context.as_ref() else {
            return 0;
        };
        match self.session_tab {
            SessionTab::Skills => snapshot.skills.len(),
            SessionTab::Permissions => snapshot.permissions.len(),
            SessionTab::Tools => snapshot.tools.len(),
            SessionTab::Model | SessionTab::Mcp => 0,
        }
    }

    /// Build the mutation request implied by activating the selected row in the
    /// active pane, or `None` when the pane is read-only or the selection is
    /// out of range. The harness applies it and replies with a fresh snapshot.
    pub fn session_activate_request(&self) -> Option<AgentRequest> {
        let snapshot = self.session_context.as_ref()?;
        match self.session_tab {
            SessionTab::Permissions => {
                let rule = snapshot.permissions.get(self.modal_index)?;
                Some(AgentRequest::RevokePermission {
                    tool: rule.tool.clone(),
                    scope: rule.scope.clone(),
                })
            }
            SessionTab::Tools => {
                let tool = snapshot.tools.get(self.modal_index)?;
                Some(AgentRequest::ToggleTool {
                    name: tool.name.clone(),
                    enabled: !tool.enabled,
                })
            }
            SessionTab::Model | SessionTab::Mcp | SessionTab::Skills => None,
        }
    }
}
