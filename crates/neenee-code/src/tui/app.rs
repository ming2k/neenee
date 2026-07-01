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

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};

use neenee_core::{
    AgentRequest, AgentResponse, ImagePart, ParentStatus, PermissionRequest,
    ProviderPickerSnapshot, Pursuit, Role, SessionOverview, TodoList,
};

use crate::tui::completion::PathScan;
use crate::tui::composer_attachments;
use crate::tui::document::{DeliveryStatus, TranscriptMessage};
use crate::tui::event_loop::resolve_focused_mut;
use crate::tui::fuzzy;
use crate::tui::layout::{InteractiveTarget, LayoutMap, ModalHitMap};
use crate::tui::providers::{
    CustomField, ProviderTemplate, RankedModel, RankedProvider, edit_fields,
    provider_models_filtered_from, providers_filtered_from,
};
use crate::tui::render::Theme;
use crate::tui::selection::{SelectionDrag, SelectionState};
use crate::tui::{ActivityTab, Modal};

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

/// Which surface owns the terminal cursor right now — the single source of
/// truth that the event loop's hide/show state machine, the immediate
/// pre-draw cursor re-sync, and the composer's `show_caret` flag all derive
/// from.
///
/// The terminal cursor is what the host terminal's IME anchors its
/// composition window to, so the owner must be exactly the one text-input
/// surface the user is typing into — or [`Self::None`] when no such surface
/// exists (a transcript step has keyboard focus, the view is zoomed into an
/// envoy task, or a read-only / decision modal is open). In the `None` case
/// the cursor is hidden so the IME has no stale anchor to bind to, which is
/// the bug that previously let the IME "drift" when a disclosure was
/// clicked mid-composition: the caret left the composer but the cursor
/// stayed visible at its old coordinate.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum CaretOwner {
    /// The live composer (no modal, no envoy zoom, no transcript-step focus).
    Composer,
    /// A modal that renders its own caret ([`Modal::owns_caret`]).
    Modal,
    /// No text-input surface is active — the cursor must be hidden.
    None,
}

/// Capturable snapshot of the main transcript's scroll position, saved when
/// zooming into a nested view and restored on return.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct ScrollSnapshot {
    pub offset: u16,
    pub follow_bottom: bool,
}

/// One frame on the focus stack: the envoy task call-id plus the parent
/// view's scroll snapshot, restored verbatim when the frame is popped.
#[derive(Clone, Debug, PartialEq)]
pub struct ZoomFrame {
    pub call_id: String,
    pub saved_scroll: ScrollSnapshot,
}

pub struct App {
    pub input: String,
    /// Structured transcript messages (semantic document model).
    pub messages: Vec<TranscriptMessage>,
    /// Version of the shared runtime buffer that `messages` was last synced
    /// from. The loop re-clones the buffer only when the runtime version moves
    /// past this, so an unchanged transcript costs no per-frame deep clone.
    /// Starts at 0 (the `Versioned` sentinel) so the first frame always syncs.
    pub messages_version: u64,
    /// Side-conversation transcript (ADR-0017). Populated only while a `/btw`
    /// side session is live; per-turn events tagged with the side `session_id`
    /// route here instead of into `messages`.
    pub side_messages: Vec<TranscriptMessage>,
    /// Companion to `messages_version` for the side buffer.
    pub side_messages_version: u64,
    /// Per-message laid-out height cache (Stage 2). Lets the transcript renderer
    /// skip re-wrapping off-screen messages, making per-frame layout O(visible)
    /// instead of O(transcript). Cleared whenever the transcript changes (a
    /// `messages_version` / `side_messages_version` bump) so a cached height is
    /// only ever read while the message's content is unchanged.
    pub layout_height_cache: crate::tui::render::HeightCache,
    /// True while the user is composing into the `/btw` side conversation
    /// (ADR-0017). Drives [`App::focused_messages`] to swap the viewed
    /// transcript to [`App::side_messages`] and reserves the side banner.
    pub in_side_view: bool,
    /// Active side `session_id`, learned from [`AgentResponse::SideViewOpened`].
    /// The response listener routes a `Turn { session_id, .. }` event into the
    /// side buffer when this matches, and into the primary buffer otherwise.
    pub side_session_id: Option<String>,
    /// Coarse primary-session status, mirrored from
    /// [`AgentResponse::ParentStatus`] for the side banner.
    pub parent_status: ParentStatus,
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
    pub sticky_rect: Option<neenee_tui::Rect>,
    /// Screen rect of the activity bar for the current frame, so clicks inside
    /// it open the Activity modal. `None` when no activity bar is shown (idle,
    /// streaming, envoy view, or chrome hidden).
    pub activity_rect: Option<neenee_tui::Rect>,
    /// Screen rect of the context-meter segment in the hint bar (the
    /// `89.2k (8%)` indicator), so a click on it opens the TokenReport modal.
    /// `None` when the hint bar or context meter is not shown.
    pub hint_context_rect: Option<neenee_tui::Rect>,
    /// Shared token-source ledger (reported vs. estimated token accounting),
    /// read by the TokenReport modal. `None` in tests that don't surface it.
    pub token_ledger: Option<Arc<neenee_core::TokenSourceLedger>>,
    /// Scroll offset of the TokenReport modal body.
    pub token_report_scroll: usize,
    /// `true` when the TokenReport modal is drilled into a single provider/model
    /// detail (per-round line items + cache efficiency); `false` = the bill list.
    pub token_report_detail: bool,
    /// Latest `/debug context` snapshot, read by the Debug inspector modal.
    /// `None` until a snapshot is received from the harness.
    pub debug_snapshot: Option<neenee_core::DebugSnapshot>,
    /// Drilled-in section of the Debug inspector (`None` = the section list).
    /// Holds the same `DebugDetail` / `DebugSection` types the renderer takes.
    pub debug_detail: neenee_tui_view::render::DebugDetail,
    /// Scroll offset of the Debug inspector body.
    pub debug_scroll: usize,
    /// Screen rect of the `todos d/t` segment on the activity bar, so a click
    /// on it opens the Activity modal directly on the Todos section. `None`
    /// when no todos are shown (empty task list or bar hidden).
    pub todos_rect: Option<neenee_tui::Rect>,
    /// Screen rect of the currently-open dismissable overlay modal (the
    /// centered panel, not the full-screen backdrop), so a click that lands
    /// outside it closes the modal — mirroring Esc. Written each render from
    /// the rect returned by the modal renderer. `None` when no modal is open,
    /// when the modal paints no full backdrop (Permission), or when it borrows
    /// the composer input and therefore must close through its own restore
    /// path (Provider / ModelEditor).
    pub modal_rect: Option<neenee_tui::Rect>,
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
    /// Stack of nested zoom frames (envoy tasks). Empty means the root
    /// conversation is shown; the top frame is the currently focused view.
    /// Each frame carries the parent's scroll snapshot, restored on exit.
    pub focus_stack: Vec<ZoomFrame>,
    pub tx: mpsc::UnboundedSender<AgentRequest>,
    pub should_quit: Arc<AtomicBool>,
    /// `/serve` hot-attach tap (ADR-0037 §7). When active, the response
    /// listener clones every `AgentResponse` into this broadcast sender so
    /// WebSocket clients receive the live stream. `None` when serve is off —
    /// zero cost (the listener's `if let` never fires).
    pub serve_tap: Arc<AsyncMutex<Option<broadcast::Sender<AgentResponse>>>>,
    /// Cancellation token for the serve listener, so `/serve stop` can
    /// shut it down cleanly. `None` when serve is inactive.
    pub serve_cancel: Option<tokio_util::sync::CancellationToken>,
    pub suggestion_index: Option<usize>,
    /// Latched whenever the user finishes a completion: an `Enter` commit (any
    /// kind), an `Esc` dismiss, **or a slash-command accept via Tab/Enter**
    /// (a terminal accept — see [`Self::accept_completion`]). While `true`,
    /// the completion popup is suppressed even if `completion_kind()` would
    /// otherwise show one — so accepting a command does not immediately flash
    /// a subcommand menu or a collapsed single-exact-match list. Cleared by
    /// the next `InsertChar` / `Backspace` (the user is editing again, so
    /// live completions are once again useful). `@path` accepts via Tab do
    /// **not** latch — Tab is meant to keep cycling path candidates.
    pub completion_dismissed: bool,
    pub custom_commands: Vec<(String, String)>,
    pub cursor_position: usize,
    pub input_scroll: usize,
    pub active_modal: Modal,
    pub modal_index: usize,
    /// Last-known screen rect of the composer. Refreshed every draw and reused
    /// between frames by the input-driven immediate cursor flush so the IME
    /// composition window is re-anchored in the *same* iteration a keystroke is
    /// handled — before the next frame is even rendered (the fix for the
    /// one-frame cursor lag that mis-anchored IME). It is only an approximation
    /// of the rect the *next* frame will compute (the footer height can change
    /// when wrapping shifts), but a follow-up full draw always lands when that
    /// happens, so the approximation is correct exactly when it matters (the
    /// non-wrap-moving keystrokes that dominate real typing).
    pub last_input_rect: neenee_tui::Rect,
    /// Whether the terminal cursor should be moved to match `cursor_position`
    /// before the next frame, eliminating the one-frame IME lag. Set by
    /// [`App::set_cursor`] (the single write site for `cursor_position`) and
    /// cleared by the event loop's immediate-flush after it syncs the backend.
    pub cursor_sync_pending: bool,
    /// The cursor visibility we last told the terminal. The event loop's
    /// hide/show state machine consults this so show/hide is a state
    /// transition (escape codes emitted only on an edge) driven by
    /// [`App::caret_visible`], not a per-frame guess.
    pub cursor_visible: bool,
    /// Body scroll offset shared by the Tools / Mcp / Skills managers
    /// ([`Modal::Tools`] / [`Modal::Mcp`] / [`Modal::Skills`]). Reset to 0 on
    /// open. Clamped (and, when `session_modal_follow` is set, auto-followed to
    /// the selection cursor) by the renderer each frame.
    pub session_scroll: usize,
    /// When true, the Tools/Mcp/Skills body scroll follows the ↑/↓ selection
    /// cursor (the default after open / navigation). Cleared the moment the
    /// user scrolls manually (wheel / page keys) so they can browse freely, and
    /// re-set the moment they navigate again.
    pub session_modal_follow: bool,
    /// Body scroll offset of the permissions manager modal. Reset to 0 each
    /// time the modal opens; clamped and auto-followed to the selection by the
    /// renderer each frame.
    pub permissions_scroll: usize,
    /// Body scroll offset of the config manager modal. Reset to 0 each time
    /// the modal opens; clamped each frame by the renderer. Selection cursor
    /// for the config root and nudge sub-page reuses [`Self::modal_index`].
    pub config_scroll: usize,
    /// Index of the skills-modal row whose detail block is expanded
    /// ([`Modal::Skills`]), or `None` when every row is collapsed. `Enter`
    /// toggles the selected row; reset to `None` each time the modal opens.
    /// The skills modal reuses [`Self::modal_index`] for its selection cursor
    /// and [`Self::session_scroll`] for its body scroll.
    pub skills_expanded: Option<usize>,
    /// Body scroll offset of the history modal (Ctrl+R). Reset to 0 each time
    /// the modal opens (and when toggling browse/search/preview); clamped and
    /// auto-followed to the selection by the renderer each frame.
    pub history_scroll: usize,
    /// When true, the history modal's body scroll follows the ↑/↓ selection
    /// cursor. Cleared on manual scroll (free browse), re-set on navigation.
    pub history_modal_follow: bool,
    /// When true, the history modal shows the full (multi-line) text of the
    /// selected entry instead of the one-line-per-row fuzzy list. Toggled by
    /// Tab; ↑/↓ re-shows the focused entry's complete prompt. `history_scroll`
    /// is reused as the per-entry scroll inside preview mode.
    pub history_preview: bool,
    /// Whether the history modal's **search sub-layer** is active. The modal
    /// opens in browse mode (`false`): a plain reverse-chronological list with
    /// no query field. Pressing `/` enters search (`true`), which borrows the
    /// composer line as a live fuzzy query; the first Esc returns to browse and
    /// the second closes the modal. See [`App::history_rows`].
    pub history_search: bool,
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
    pub current_pursuit: Option<Pursuit>,
    /// Latest session-context snapshot for the Tools / Mcp / Skills /
    /// Permissions managers, or `None` before the first `QuerySessionContext`
    /// round-trip completes. Refreshed each frame from the response listener.
    pub session_context: Option<neenee_core::SessionContextSnapshot>,
    /// Live nudge config snapshot, mirrored from
    /// `AgentResponse::NudgeConfigUpdated` each frame. The `/config` modal
    /// reads this to render the current thresholds and enabled state; edits
    /// go out as `AgentRequest::UpdateNudgeConfig`.
    pub nudge_config: neenee_core::NudgeConfig,
    pub loop_status: String,
    pub activity_status: String,
    /// Whether write-tool permission prompts are bypassed this session
    /// (`--unattended` / `/unattended on`). Mirrored from the harness
    /// snapshot; surfaced by the hint bar's flat `UNATTENDED` label (warning
    /// tone) below the input so the elevated state is unmissable.
    pub unattended: bool,
    /// Unified task list, mirrored from `AgentResponse::TodosUpdated`. Shown
    /// inside the Activity modal (and no longer pinned above the input box) so
    /// the footer reclaims the vertical space. `None` (or an empty list)
    /// hides it. A plan approved via `plan_exit` seeds this list from its
    /// `##` headings.
    pub todos: Option<TodoList>,
    /// Harness turn counter, mirrored each frame. Surfaced inside the
    /// Activity modal as `turn N` (the activity bar itself no longer shows
    /// the structural counters — it surfaces status/plan/elapsed and is the
    /// click target that opens the modal).
    pub round_count: u64,
    /// Current tool round within the active turn (1-indexed for display:
    /// `0` means the turn has started but no model request has fired yet —
    /// e.g. the "queued" / "preparing context" phase). Mirrored each frame
    /// from the response listener; shown in the Activity modal as
    /// `turn N · round M · <status>`.
    pub current_turn: u64,
    /// Session-review alert (ADR-0016), or empty when inactive. While
    /// non-empty the activity bar appends a `⚠ <alert> — Esc to interrupt`
    /// segment. Mirrored each frame from the response listener.
    pub review_alert: String,
    /// Wall-clock instant the current turn started, or `None` between turns.
    /// Drives the muted `<elapsed>` segment in the activity bar.
    pub turn_started_at: Option<std::time::Instant>,
    /// Active tab inside the Activity modal ([`Modal::Activity`]).
    /// Ignored while any other modal is open.
    pub activity_tab: ActivityTab,
    /// Scroll offset inside `Modal::Activity`. Reset to 0 each time the modal
    /// opens; clamped each frame by the modal's body renderer.
    pub activity_scroll: usize,
    /// Scroll offset inside `Modal::Help`. Reset to 0 each time the modal opens;
    /// clamped each frame by the modal's body renderer. The keybinding list
    /// overflows a typical terminal, so this is what keeps the lower sections
    /// reachable — the renderer used to take a throwaway `&mut 0`, leaving the
    /// modal unscrollable.
    pub help_scroll: usize,
    pub pending_permission: Option<PermissionRequest>,
    /// The pending interactive-input request (L3.5 β) from an interactive
    /// `bash` command, or `None`. Set when a `RoundEvent::InputRequest` arrives;
    /// the input-injection modal reads it for its prompt/command/secret.
    pub pending_input: Option<neenee_core::InputRequest>,
    /// The open question (ask_user) modal's self-contained MVU state, or
    /// `None` when no question modal is open. Replaces the four separate
    /// `question_*` fields that previously scattered the modal's state across
    /// `App`; all interaction now flows through `QuestionModel::update`.
    pub question: Option<crate::tui::question_model::QuestionModel>,
    /// Scroll offset inside `Modal::Question`. Reset to 0 each time a question
    /// modal opens; clamped each frame by the modal's body renderer and, when
    /// `question_modal_follow` is set, nudged so the highlighted option stays on
    /// screen.
    pub question_scroll: usize,
    /// When true, the question modal's body scroll follows the ↑/↓ option
    /// highlight (the default after open / navigation). Cleared the moment the
    /// user scrolls manually (wheel / page keys) so they can browse a long
    /// option list freely, and re-set the moment they navigate again. Mirrors
    /// `session_modal_follow` / `history_modal_follow`.
    pub question_modal_follow: bool,
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
    /// The in-progress draft the user was composing when they first pressed
    /// ↑ to walk history. Restored when they press ↓ past the newest history
    /// entry, so a stray ↑ no longer loses what they were typing. Cleared on
    /// send. Distinct from `stashed_input`, which is borrowed by modal flows.
    pub history_draft: String,
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
    /// Modal-local click targets for the current frame.
    pub modal_hit_map: ModalHitMap,
    /// Message index of the step (tool step or reasoning trace) whose header
    /// currently rests under the mouse pointer (inline or sticky pinned), so
    /// the next draw lights it up to the intermediate hover tone as a click
    /// affordance. `None` whenever the pointer is elsewhere or an overlay
    /// modal is open.
    pub hovered_step: Option<usize>,
    /// Global tool-step density (false = Compact default, true = Comfortable:
    /// new tool steps spawn expanded). Shared with the response listener.
    pub tool_density: Arc<AtomicBool>,
    /// Which layout strategy arranges the transcript message stream. Selected
    /// via `[tui] transcript_layout`; defaults to Compact (the original
    /// flush-stack layout). See `crate::tui::render::layout::Strategy`.
    pub transcript_layout: crate::tui::render::layout::Strategy,
    /// Keyboard-focused activatable target in the current frame, and the TUI's
    /// only navigation state — there is no separate "browse mode". `None` means
    /// every key has its ordinary input-box meaning (typing flows into the
    /// prompt). `Some` means a transcript step is highlighted: `Ctrl+↑`/`Ctrl+↓`
    /// (or bare `↑`/`↓`) cycle it, `Enter` activates it, and `Esc` clears it.
    /// Mouse hover/click is an acceleration path onto the same state.
    pub focused_target: Option<InteractiveTarget>,
    /// Show a brief "copied" toast. Held until this deadline elapses so the
    /// duration is wall-clock consistent regardless of the event-loop cadence.
    pub copy_toast_until: Option<std::time::Instant>,
    pub copy_toast_message: String,
    pub copy_toast_failed: bool,
    /// Ticks remaining in which a second Ctrl+C quits.
    pub ctrl_c_armed_ticks: u8,
    /// Ticks remaining in which a second Esc interrupts the running task.
    pub esc_armed_ticks: u8,
    /// Epoch the breathing indicator is timed against. The spinner phase is
    /// derived from wall-clock elapsed time since this instant rather than a
    /// per-frame counter, so the breathing cadence stays constant regardless of
    /// how often the loop redraws (mouse movement, streaming, paste, etc. all
    /// wake the loop at irregular intervals and would otherwise jitter it).
    pub spinner_epoch: std::time::Instant,
    /// Input stashed while the API-key modal borrows the input line.
    pub stashed_input: String,
    /// Provider id targeted by the unified key editor ([`Modal::ModelEditor`]).
    pub editor_target: Option<String>,
    /// Which editor field is focused. `0` = API key (text entry); `1` = effort
    /// (←/→ cycling, Anthropic only); `2` = thinking (Space toggle, Anthropic
    /// only). The effort/thinking rows are only shown for the Anthropic
    /// provider, so `editor_field` is clamped to `0` otherwise.
    pub editor_field: u8,
    /// API-key buffer for the editor (the input line is borrowed for the
    /// focused field).
    pub editor_key: String,
    /// Wire model id the key editor will activate once a key is entered (carried
    /// from the stage-2 selection or the provider's default; not user-editable).
    pub editor_model: String,
    /// When true, [`Modal::ModelEditor`] edits the selected provider model's
    /// channel settings only (currently Anthropic effort/thinking), not the
    /// provider API key or active provider.
    pub editor_model_settings_only: bool,
    /// When `editor_model_settings_only` is true, whether the edited model is
    /// **built-in** (served by a built-in provider like `anthropic`). A built-in
    /// model's per-model reasoning knobs persist to the `[model_reasoning]`
    /// table via `EditModelReasoning`; a user-defined model's knobs persist to
    /// its channel via `EditProviderModel` (ADR-0045).
    pub editor_target_is_builtin: bool,
    /// Current reasoning-effort selection in the key editor, as a lowercase wire
    /// string (`"low"`/`"medium"`/`"high"`/`"xhigh"`/`"max"`). Defaults to
    /// `"high"` (the upstream wire default); cycled with ←/→. Only meaningful
    /// when [`Self::editor_target`] is `"anthropic"`. Sent as `SwitchProvider`'s
    /// `effort` on submit.
    pub editor_effort: String,
    /// Current extended-thinking on/off selection in the key editor. Defaults
    /// to `true` (adaptive thinking on — the recommended mode for Claude).
    /// Toggled with Space; orthogonal to effort. Only meaningful when
    /// [`Self::editor_target`] is `"anthropic"`. Sent as `SwitchProvider`'s
    /// `thinking` on submit.
    pub editor_thinking: bool,
    /// Focused field of the provider editor ([`Modal::CustomProvider`]) as an
    /// index into [`Self::custom_fields`] — the per-template visible field set
    /// (Name / Base URL / Token / Model). The focused field always borrows the
    /// composer line; the Model field borrows it as a live filter query.
    pub custom_field: u8,
    /// The ordered visible fields of the provider editor, chosen by the active
    /// template (create) or the edited provider's protocol (edit). Empty when no
    /// editor is open.
    pub custom_fields: Vec<CustomField>,
    /// Wire protocol of the provider being created/edited (`"openai"` |
    /// `"anthropic"` | `"gemini"`), carried from the template or the edited
    /// provider rather than chosen with a protocol picker.
    pub custom_protocol_wire: String,
    /// Models seeded by the active template (create mode). Submitted as the
    /// provider's model list unless the editor exposes a free-text Model field
    /// (then the single typed model is submitted instead). Empty in edit mode.
    pub custom_models: Vec<String>,
    /// Base URL placeholder for the active template (the expected endpoint shape).
    pub custom_url_hint: String,
    /// Highlight index into the live suggestion list for the provider editor's
    /// Model **filter** field (type to filter, `↑/↓` to move, committed live).
    pub custom_suggest_index: usize,
    /// When `Some(id)`, the provider editor is **editing** the existing user
    /// provider `id` (meta only: Name/Base URL/Token; models stay managed in the
    /// stage-2 list). `None` is create mode.
    pub custom_edit_id: Option<String>,
    /// Provider-editor buffers holding the unfocused text fields (the focused one
    /// lives in the borrowed composer line). Name / Base URL / Token / Model /
    /// Effort.
    pub custom_name: String,
    pub custom_base_url: String,
    pub custom_token: String,
    pub custom_model: String,
    /// Selected row of the provider-template chooser ([`Modal::ProviderTemplate`]),
    /// indexing [`crate::tui::PROVIDER_TEMPLATES`]. Cycled with `↑/↓`.
    pub template_choice: usize,
    /// Whether the model picker's **search sub-layer** is active. The picker
    /// ([`Modal::Provider`]) opens in browse mode (`false`): a plain ranked list
    /// with no query field. Pressing `/` enters search (`true`), which borrows
    /// the composer line as a live fuzzy query; the first Esc returns to browse
    /// and the second closes the modal. Mirrors [`Self::history_search`]. See
    /// [`Self::provider_models_filtered`].
    pub model_search: bool,
    /// The two-stage provider picker's current stage. `None` is **stage 1**, the
    /// provider list; `Some(row_idx)` is **stage 2**, the model sub-list for the
    /// snapshot row at `row_idx` (reached by activating a multi-model provider).
    /// Esc in stage 2 returns to stage 1; Esc in stage 1 closes the modal. Reset
    /// to `None` whenever the picker opens or closes. See
    /// [`Self::providers_filtered`] and [`Self::provider_models_filtered`].
    pub picker_provider: Option<usize>,
    /// Provider id targeted by the **add-model** overlay ([`Modal::AddModel`]),
    /// opened from a custom provider's stage-2 list. `None` when the overlay is
    /// closed.
    pub add_model_provider: Option<String>,
    /// Index into the add-model overlay's candidate list. The last index is the
    /// synthetic "Custom…" slot (free-text id in the borrowed input line).
    /// Cycled with `←/→`.
    pub add_model_choice: usize,
    /// Body scroll offset of the model picker. Reset to 0 each time the modal
    /// opens (and when toggling browse/search); clamped and auto-followed to the
    /// selection by the renderer each frame. Mirrors [`Self::history_scroll`].
    pub model_scroll: usize,
    /// When true, the model picker's body scroll follows the ↑/↓ selection
    /// cursor. Cleared on manual scroll (free browse), re-set on navigation.
    pub model_modal_follow: bool,
    /// Lowercase provider name → whether a usable API key is configured.
    pub key_status: HashMap<String, bool>,
    /// Live model-picker snapshot (default id + per-model favorite / key-ready
    /// / last-used). Drives the `/provider` picker's rendering and sort order
    /// Refreshed from the response listener each frame.
    pub provider_picker: ProviderPickerSnapshot,
    /// Theme.
    pub theme: Theme,
    /// User-supplied ASCII logo lines loaded at startup from
    /// `$XDG_CONFIG_HOME/neenee/logo.txt` (clamped to the empty-state bounding
    /// box). `None` when no user logo is present → built-in wordmark is used.
    /// Passed into the empty-state hero via `TranscriptView::logo`.
    pub logo: Option<Vec<String>>,
}

impl App {
    pub fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor_position)
            .unwrap_or(self.input.len())
    }

    /// Set the input caret position and mark the terminal cursor as needing an
    /// immediate re-sync before the next frame.
    ///
    /// This is the **single sanctioned write site** for `cursor_position`.
    /// Routing every caret move through it guarantees the event loop's
    /// immediate-flush (which re-anchors the IME composition window in the same
    /// iteration as the keystroke) always fires — a raw `app.cursor_position =
    /// …` would silently skip the flush and re-introduce the one-frame lag.
    pub fn set_cursor(&mut self, pos: usize) {
        self.cursor_position = pos;
        self.cursor_sync_pending = true;
    }

    /// Set the input caret to the end of `self.input` (common case after a
    /// programmatic input replacement: history navigation, modal restore,
    /// paste). Equivalent to `set_cursor(self.input.chars().count())` but
    /// reads as intent at the call site.
    pub fn set_cursor_end(&mut self) {
        let end = self.input.chars().count();
        self.set_cursor(end);
    }

    /// Record the composer's screen rect as observed during the latest draw, so
    /// the input-driven immediate cursor flush can place the caret without
    /// waiting for the next frame.
    pub fn observe_input_rect(&mut self, rect: neenee_tui::Rect) {
        self.last_input_rect = rect;
    }

    /// Record that the caret moved without going through [`App::set_cursor`]
    /// (the only legitimate caller is the input handler, which mutates
    /// `cursor_position` in place for performance and then reports the new
    /// value). Marks the immediate flush pending.
    pub fn note_cursor_moved(&mut self) {
        self.cursor_sync_pending = true;
    }

    /// The single source of truth for which surface owns the terminal cursor
    /// this frame. See [`CaretOwner`].
    ///
    /// This is a pure function of (`active_modal`, `focused_target`,
    /// `focus_stack`) — never of the selection, which is folded in separately
    /// by [`Self::caret_visible`] because a selection hides the cursor
    /// regardless of who owns it. Keeping ownership and selection-appearance
    /// decoupled is what lets the event loop distinguish "reposition the
    /// composer's caret" (owner = `Composer`, no selection) from "hide it"
    /// (owner = `Composer` but a selection is active) without re-deriving
    /// either from raw fields.
    pub fn caret_owner(&self) -> CaretOwner {
        if self.active_modal != Modal::None {
            return if self.active_modal.owns_caret() {
                CaretOwner::Modal
            } else if self.active_modal == Modal::Question
                && self
                    .question
                    .as_ref()
                    .is_some_and(|q| q.is_other_highlighted())
            {
                // The Question modal is normally a decision sheet (no caret).
                // But when the synthetic "Other" free-text row is highlighted
                // it becomes a real text-input surface, so it must own the
                // terminal cursor for that one state — otherwise the host IME
                // has no coordinate to anchor its composition window to. This
                // is the only state-dependent ownership; every other modal's
                // ownership is static via `Modal::owns_caret`.
                CaretOwner::Modal
            } else {
                CaretOwner::None
            };
        }
        // No modal: the composer owns the caret unless a transcript step has
        // keyboard focus or we are zoomed into an envoy task (which has no
        // input line at all — its footer collapses to zero height).
        if self.focused_target.is_some() || self.in_envoy_view() {
            CaretOwner::None
        } else {
            CaretOwner::Composer
        }
    }

    /// Whether the terminal cursor should be visible right now —
    /// [`Self::caret_owner`] plus the one extra rule that an active text
    /// selection hides the cursor (a block cursor would clash with the
    /// selection background). This is what every cursor site consults; no
    /// call site should re-derive visibility from raw fields.
    pub fn caret_visible(&self) -> bool {
        !self.selection.is_active() && self.caret_owner() != CaretOwner::None
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
        self.set_cursor_end();
        if !dispatch.images.is_empty() {
            self.pending_images = dispatch.images;
        }
        if !dispatch.text_pastes.is_empty() {
            self.pending_text_pastes = dispatch.text_pastes;
        }
        // Clear the history cursor so a subsequent ↓ returns to an empty
        // input rather than to the now-stale history entry.
        self.history_index = None;
        // Programmatic input replacement, like history navigation: latch the
        // completion dismissal so a recalled slash command doesn't re-open its
        // popup until the next real edit (InsertChar/Backspace clears it).
        self.suggestion_index = None;
        self.completion_dismissed = true;
        true
    }

    /// Splice the `idx`-th live completion's label into [`App::input`] over
    /// its `[replace_start, replace_end)` byte range, landing the cursor
    /// just past the inserted text. Shared by `Tab` cycling and `Enter`
    /// commit.
    ///
    /// **Slash commands are terminal accepts.** Accepting a `/command` is a
    /// commit: no trailing space is appended, the highlight is cleared, and
    /// [`App::completion_dismissed`] is latched so the popup stays hidden
    /// until the next edit. This unifies Tab and Enter — a `/pursue ` (with
    /// the space) would immediately match the subcommand prefix and
    /// re-trigger the menu (defeating the point of accepting), and once a
    /// slash label replaces the whole input the candidate list collapses to
    /// the single exact match anyway, so cycling has nothing to cycle
    /// through. The user opts back into completion by editing the input
    /// (clearing the latch) or, for subcommand discovery, by typing a space.
    ///
    /// **`@path` mentions keep cycling.** Files splice inline, so multiple
    /// candidates survive an accept and Tab is meant to walk them; the popup
    /// therefore re-opens for path accepts and no latch is set. Directories
    /// end in `/` and also skip the trailing space so the popup re-triggers
    /// on the dir's contents.
    pub fn accept_completion(&mut self, idx: usize) {
        let completions = self.completions();
        let Some(comp) = completions.get(idx) else {
            return;
        };
        let replace_start = comp.replace_start;
        let replace_end = comp.replace_end;
        let mut label = comp.label.clone();
        let is_slash = label.starts_with('/');
        let is_dir = label.ends_with('/');
        // File accept: append a trailing space so the user can keep typing
        // their message (matches opencode's splice behaviour). Directories
        // end in `/` (re-trigger on the dir's contents) and slash commands
        // are terminal (see the doc comment), so neither gets the space.
        if !is_dir && !is_slash {
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
        self.set_cursor(self.input[..cursor_byte].chars().count());
        // Drop the cached project scan so newly-created files become
        // visible on the next `@` mention without a restart.
        self.path_scan_cache = None;
        // A slash-command accept is a commit: exit completion so the popup
        // does not re-open on the just-spliced label (which would collapse
        // to a single exact match and, with a trailing space, fire the
        // subcommand menu). Applies equally to Tab and Enter since both
        // route through here. `@path` accepts stay live for Tab cycling.
        if is_slash {
            self.suggestion_index = None;
            self.completion_dismissed = true;
        }
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
        if let Some(message_idx) = self.sticky_step
            && let Some(message) = self.focused_messages().get(message_idx)
        {
            let target = if message.is_thinking() {
                InteractiveTarget::thinking(message_idx)
            } else if message.is_tool_step() || message.is_envoy_task() {
                InteractiveTarget::tool_step(message_idx)
            } else {
                return targets;
            };
            if !targets.contains(&target) {
                targets.insert(0, target);
            }
        }
        targets
    }

    pub(crate) fn retain_visible_focused_target(&mut self) {
        if self.active_modal != Modal::None {
            self.focused_target = None;
            return;
        }
        if let Some(target) = self.focused_target
            && !self.visible_interactive_targets().contains(&target)
        {
            self.focused_target = None;
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

    /// Whether the view is currently zoomed into an envoy task.
    pub fn in_envoy_view(&self) -> bool {
        !self.focus_stack.is_empty()
    }

    /// The message slice currently in view: the `/btw` side transcript when
    /// the side view is active (ADR-0017), the focused envoy task's child
    /// messages when zoomed, or the root conversation otherwise.
    pub fn focused_messages(&self) -> &[TranscriptMessage] {
        if self.in_side_view {
            return &self.side_messages;
        }
        let Some(frame) = self.focus_stack.last() else {
            return &self.messages;
        };
        self.messages
            .iter()
            .find_map(|message| {
                if message.is_envoy_task()
                    && message.tool_step_call_id() == Some(frame.call_id.as_str())
                {
                    message.envoy_children()
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

    /// Zoom into an envoy task's child messages.
    pub fn enter_envoy(&mut self, call_id: String) {
        let saved_scroll = ScrollSnapshot {
            offset: self.scroll,
            follow_bottom: self.follow_bottom,
        };
        self.focus_stack.push(ZoomFrame {
            call_id,
            saved_scroll,
        });
        self.reset_view_state();
    }

    /// Return from the current envoy view to its parent. Returns true if a
    /// view was actually popped.
    pub fn exit_envoy(&mut self) -> bool {
        if let Some(frame) = self.focus_stack.pop() {
            self.reset_view_state();
            self.scroll = frame.saved_scroll.offset;
            self.follow_bottom = frame.saved_scroll.follow_bottom;
            true
        } else {
            false
        }
    }

    /// Enter the `/btw` side conversation view (ADR-0017). The side transcript
    /// ([`App::side_messages`]) becomes the viewed stream and a top banner
    /// reports the primary session's coarse status. Reuses the envoy
    /// zoom's `reset_view_state` so the swap feels identical to focusing a
    /// task step.
    pub fn enter_side_view(&mut self, side_id: String) {
        self.side_session_id = Some(side_id);
        self.in_side_view = true;
        self.side_messages.clear();
        self.parent_status = ParentStatus::Idle;
        self.reset_view_state();
    }

    /// Leave the `/btw` side view and return to the primary transcript. The
    /// side buffer is dropped (the side session file remains on disk,
    /// recoverable via `/sessions`).
    pub fn exit_side_view(&mut self) {
        self.in_side_view = false;
        self.side_session_id = None;
        self.side_messages.clear();
        self.reset_view_state();
    }

    /// Cycle to the previous (`dir < 0`) or next (`dir > 0`) sibling envoy
    /// task at the current focus level. No-op when not in an envoy view or
    /// when there are no siblings.
    pub fn cycle_sibling(&mut self, dir: i8) {
        let Some(current) = self.focus_stack.last() else {
            return;
        };
        let current_id = current.call_id.clone();
        let task_ids: Vec<String> = self
            .messages
            .iter()
            .filter_map(|message| {
                if message.is_envoy_task() {
                    message.tool_step_call_id().map(String::from)
                } else {
                    None
                }
            })
            .collect();
        let Some(idx) = task_ids.iter().position(|id| *id == current_id) else {
            return;
        };
        if task_ids.len() < 2 {
            return;
        }
        let n = task_ids.len() as isize;
        let next = ((idx as isize + dir as isize).rem_euclid(n)) as usize;
        if let Some(frame) = self.focus_stack.last_mut() {
            frame.call_id = task_ids[next].clone();
        }
        self.reset_view_state();
    }

    /// Rows shown in the Ctrl+R history modal, as `(original_index, FuzzyMatch)`
    /// pairs. The single source of truth for navigation (Up/Down clamp), Enter
    /// accept, and rendering — they all index into this same vector so the
    /// cursor never lands on a row the user cannot see.
    ///
    /// In **browse** mode (or in search mode before the user types anything) the
    /// list is **reverse-chronological** — newest first — with empty matches (no
    /// highlight). Once a query is present in **search** mode the rows are the
    /// fuzzy-ranked matches, best score first, with input order as the stable
    /// tiebreaker. Recomputed from scratch each call: history is small and this
    /// runs at most a few times per frame, so caching would only add stale-state
    /// risk.
    pub fn history_rows(&self) -> Vec<(usize, fuzzy::FuzzyMatch)> {
        if !self.history_search || self.input.is_empty() {
            return (0..self.input_history.len())
                .rev()
                .map(|i| {
                    (
                        i,
                        fuzzy::FuzzyMatch {
                            score: 0,
                            positions: Vec::new(),
                        },
                    )
                })
                .collect();
        }
        let mut ranked = fuzzy::rank(&self.input_history, &self.input);
        fuzzy::sort_by_score(&mut ranked);
        ranked
    }

    /// Record `entry` in the cross-session input history: dedup against the
    /// last entry, reset the up/down recall cursor, and persist the new entry
    /// to disk immediately (off-thread) so it survives an unclean exit and is
    /// visible to concurrent sessions right away rather than only on exit.
    pub fn record_input_history(&mut self, entry: String) {
        self.history_index = None;
        if entry.is_empty() || self.input_history.last() == Some(&entry) {
            return;
        }
        self.input_history.push(entry.clone());
        // `save_history` lock+merges into the on-disk union, so persisting just
        // the new entry is enough and cheap. Off-thread: the write takes a file
        // lock and must not block the event loop.
        tokio::task::spawn_blocking(move || {
            let _ = neenee_store::config::Config::save_history(std::slice::from_ref(&entry));
        });
    }

    /// Tear down the history modal's borrowed state: hand the parked composer
    /// draft back, drop any filter query, and clear the search/preview
    /// sub-flags. Shared by the Esc (`CloseModal`) and click-outside dismiss
    /// paths so the two can never drift. Does **not** touch `active_modal` —
    /// the caller owns that transition.
    pub fn restore_history_draft(&mut self) {
        self.input = std::mem::take(&mut self.stashed_input);
        self.set_cursor_end();
        self.input_scroll = 0;
        self.suggestion_index = None;
        self.modal_index = 0;
        self.history_search = false;
        self.history_preview = false;
    }

    /// Tear down the model picker's borrowed state: hand the parked composer
    /// draft back, drop any filter query, and clear the search/scroll sub-flags.
    /// Shared by the Esc (`CloseModal`), click-outside dismiss, and activation
    /// paths so they can never drift. Mirrors [`Self::restore_history_draft`];
    /// does **not** touch `active_modal` — the caller owns that transition.
    pub fn restore_model_draft(&mut self) {
        self.input = std::mem::take(&mut self.stashed_input);
        self.set_cursor_end();
        self.input_scroll = 0;
        self.suggestion_index = None;
        self.modal_index = 0;
        self.model_search = false;
        self.picker_provider = None;
        self.model_scroll = 0;
        self.model_modal_follow = true;
    }

    /// Open the provider-template chooser — the "＋ Add provider" entry point.
    /// The chat draft is already parked in `stashed_input` (the picker stashed it
    /// on open); the chooser is a pure list, so the composer line stays clear.
    pub fn open_provider_template_chooser(&mut self) {
        self.active_modal = Modal::ProviderTemplate;
        self.template_choice = 0;
        self.input.clear();
        self.set_cursor(0);
        self.picker_provider = None;
    }

    /// Move the template-chooser selection, wrapping at the ends.
    pub fn move_template_choice(&mut self, forward: bool) {
        let n = crate::tui::PROVIDER_TEMPLATES.len();
        if n == 0 {
            return;
        }
        self.template_choice = if forward {
            (self.template_choice + 1) % n
        } else {
            (self.template_choice + n - 1) % n
        };
    }

    /// Open the provider editor seeded from `template` (create mode) on the Name
    /// field. The composer line is borrowed for the focused Name field.
    pub fn open_custom_provider_editor(&mut self, template: &ProviderTemplate) {
        self.active_modal = Modal::CustomProvider;
        self.custom_edit_id = None;
        self.custom_fields = template.fields();
        self.custom_field = 0;
        self.custom_protocol_wire = template.protocol.to_string();
        self.custom_models = template.models.iter().map(|m| m.to_string()).collect();
        self.custom_url_hint = template.url_hint.to_string();
        self.custom_suggest_index = 0;
        self.custom_name.clear();
        self.custom_base_url.clear();
        self.custom_token.clear();
        // Default the (optional) Model field to the first candidate so the
        // OpenAI-compatible template submits a usable model even if left untouched.
        self.custom_model = self
            .custom_model_candidates()
            .first()
            .map(|m| m.to_string())
            .unwrap_or_default();
        self.input.clear();
        self.set_cursor(0);
        self.picker_provider = None;
    }

    /// Open the provider editor in **edit** mode for an existing user provider,
    /// pre-filling its metadata. The Model field is hidden (models are managed in
    /// the stage-2 list), so only Name / Base URL / Token show — and Base URL is
    /// omitted for a native-Gemini provider.
    pub fn open_edit_provider_editor(
        &mut self,
        id: String,
        name: String,
        protocol: String,
        base_url: String,
    ) {
        self.active_modal = Modal::CustomProvider;
        self.custom_edit_id = Some(id);
        self.custom_fields = edit_fields(&protocol);
        self.custom_field = 0;
        self.custom_protocol_wire = protocol;
        self.custom_models.clear();
        self.custom_url_hint.clear();
        self.custom_suggest_index = 0;
        self.custom_name = name.clone();
        self.custom_base_url = base_url;
        self.custom_token.clear();
        self.custom_model.clear();
        self.input = name;
        self.set_cursor_end();
        self.picker_provider = None;
    }

    /// Whether the provider editor is editing an existing provider.
    pub fn custom_is_editing(&self) -> bool {
        self.custom_edit_id.is_some()
    }

    /// The currently focused editor field, or `None` when no editor is open.
    pub fn current_custom_field(&self) -> Option<CustomField> {
        self.custom_fields.get(self.custom_field as usize).copied()
    }

    /// Number of visible fields the editor exposes for the active template.
    fn custom_field_count(&self) -> u8 {
        self.custom_fields.len().max(1) as u8
    }

    /// The registry model ids matching the editor's protocol wire format — the
    /// Model filter field's candidate pool.
    pub fn custom_model_candidates(&self) -> Vec<&'static str> {
        crate::tui::protocol_model_candidates(&self.custom_protocol_wire)
    }

    /// The model suggestions matching the live filter (`self.input` while the
    /// Model field is focused): protocol candidates that fuzzy-match, plus the
    /// raw typed text as a custom id when it is not already a candidate.
    pub fn custom_model_suggestions(&self) -> Vec<String> {
        let q = self.input.trim();
        let mut out: Vec<String> = self
            .custom_model_candidates()
            .into_iter()
            .filter(|id| {
                q.is_empty()
                    || id.contains(q)
                    || crate::tui::fuzzy::fuzzy_match(&crate::tui::model_display_name(id), q)
                        .is_some()
            })
            .map(|s| s.to_string())
            .collect();
        if !q.is_empty() && !out.iter().any(|m| m == q) {
            out.push(q.to_string());
        }
        out
    }

    /// Commit the highlighted Model suggestion into `custom_model`. No-op off the
    /// Model field (the only filter field).
    fn commit_custom_suggestion(&mut self) {
        if self.current_custom_field() == Some(CustomField::Model) {
            let suggestions = self.custom_model_suggestions();
            if let Some(value) = suggestions.get(self.custom_suggest_index) {
                self.custom_model = value.clone();
            }
        }
    }

    /// Move the Model suggestion highlight, committing the newly-highlighted
    /// suggestion live. No-op when the Model field is not focused.
    pub fn move_custom_suggestion(&mut self, forward: bool) {
        if self.current_custom_field() != Some(CustomField::Model) {
            return;
        }
        let len = self.custom_model_suggestions().len();
        if len == 0 {
            return;
        }
        self.custom_suggest_index = if forward {
            (self.custom_suggest_index + 1) % len
        } else {
            (self.custom_suggest_index + len - 1) % len
        };
        self.commit_custom_suggestion();
    }

    /// React to a change in the Model filter query: reset the highlight to the
    /// best (first) match and commit it.
    pub fn on_custom_filter_changed(&mut self) {
        if self.current_custom_field() == Some(CustomField::Model) {
            self.custom_suggest_index = 0;
            self.commit_custom_suggestion();
        }
    }

    /// Save the composer line into the focused text field's buffer (Name / Base
    /// URL / Token). The Model field is a filter whose value is already committed
    /// live, so its transient query is discarded.
    pub fn stash_custom_field(&mut self) {
        let value = std::mem::take(&mut self.input);
        match self.current_custom_field() {
            Some(CustomField::Name) => self.custom_name = value,
            Some(CustomField::BaseUrl) => self.custom_base_url = value,
            Some(CustomField::Token) => self.custom_token = value,
            _ => {} // Model filter field: value already committed live.
        }
    }

    /// Load the focused field into the composer line: the buffer for a text
    /// field, or a fresh (empty) filter for the Model field, with the suggestion
    /// highlight positioned on the current committed value.
    pub fn load_custom_field(&mut self) {
        self.input = match self.current_custom_field() {
            Some(CustomField::Name) => self.custom_name.clone(),
            Some(CustomField::BaseUrl) => self.custom_base_url.clone(),
            Some(CustomField::Token) => self.custom_token.clone(),
            _ => String::new(),
        };
        self.set_cursor_end();
        if self.current_custom_field() == Some(CustomField::Model) {
            self.custom_suggest_index = self
                .custom_model_suggestions()
                .iter()
                .position(|v| v == &self.custom_model)
                .unwrap_or(0);
        }
    }

    /// Move the provider editor focus (`Tab` / `BackTab`), wrapping across the
    /// active template's visible fields.
    pub fn cycle_custom_field(&mut self, forward: bool) {
        self.stash_custom_field();
        let n = self.custom_field_count();
        self.custom_field = if forward {
            (self.custom_field + 1) % n
        } else {
            (self.custom_field + n - 1) % n
        };
        self.load_custom_field();
    }

    /// Park the composer draft into `stashed_input` and clear the live line so
    /// the input-injection modal (L3.5 β) can borrow it for free-text entry.
    /// Mirrors the stash half of the provider/history pickers.
    pub fn park_input_draft(&mut self) {
        self.stashed_input = std::mem::take(&mut self.input);
        self.set_cursor(0);
        self.input_scroll = 0;
        self.suggestion_index = None;
    }

    /// Tear down the input-injection modal's borrowed state: hand the parked
    /// composer draft back. Does **not** touch `active_modal`.
    pub fn restore_input_draft(&mut self) {
        self.input = std::mem::take(&mut self.stashed_input);
        self.set_cursor_end();
        self.input_scroll = 0;
        self.suggestion_index = None;
        self.modal_index = 0;
    }

    /// The active fuzzy query for the picker: the borrowed composer line while
    /// the search sub-layer is active, else empty (browse mode shows every row).
    fn picker_query(&self) -> &str {
        if self.model_search {
            self.input.trim()
        } else {
            ""
        }
    }

    /// Compute the **stage-1** provider rows. Delegates to
    /// [`providers_filtered_from`] so the input handler and the renderer share
    /// one filter+sort implementation.
    pub fn providers_filtered(&self) -> Vec<RankedProvider> {
        providers_filtered_from(&self.provider_picker, self.picker_query())
    }

    /// Compute the **stage-2** model rows for the drilled-into provider
    /// ([`Self::picker_provider`]). Empty in stage 1 (no provider selected).
    pub fn provider_models_filtered(&self) -> Vec<RankedModel> {
        match self.picker_provider {
            Some(idx) => {
                provider_models_filtered_from(&self.provider_picker, idx, self.picker_query())
            }
            None => Vec::new(),
        }
    }

    /// Whether the drilled-into provider (stage 2) is user-defined, so the model
    /// list offers add/remove. `false` in stage 1 or for built-in providers.
    pub fn picker_provider_is_custom(&self) -> bool {
        self.picker_provider
            .and_then(|idx| self.provider_picker.rows.get(idx))
            .map(|row| !row.builtin)
            .unwrap_or(false)
    }

    /// The model candidate list for the add-model overlay: the registry models
    /// matching the targeted custom provider's wire format (derived from its
    /// active model). Empty when the overlay is closed or the provider is gone.
    pub fn add_model_candidates(&self) -> Vec<&'static str> {
        let Some(id) = self.add_model_provider.as_deref() else {
            return Vec::new();
        };
        let Some(row) = self.provider_picker.rows.iter().find(|r| r.id == id) else {
            return Vec::new();
        };
        let format = neenee_core::resolve_model(&row.model).format;
        let wire = match format {
            neenee_core::WireFormat::AnthropicCompat => "anthropic",
            neenee_core::WireFormat::Gemini => "gemini",
            neenee_core::WireFormat::OpenAiCompat => "openai",
        };
        crate::tui::protocol_model_candidates(wire)
    }

    /// Number of selectable rows in the picker's current stage — stage-2 model
    /// rows when drilled into a provider, else stage-1 provider rows. Used to
    /// clamp the ↑/↓ selection cursor.
    pub fn picker_row_count(&self) -> usize {
        if self.picker_provider.is_some() {
            // Custom providers gain a trailing synthetic "＋ Add model" row.
            let models = self.provider_models_filtered().len();
            if self.picker_provider_is_custom() {
                models + 1
            } else {
                models
            }
        } else {
            // Stage 1 has a trailing synthetic "＋ Add provider" row after the
            // provider rows, so it is always selectable even with no matches.
            self.providers_filtered().len() + 1
        }
    }

    /// Whether `modal_index` is on the stage-1 "＋ Add provider" row (the
    /// synthetic trailing row, index == provider count). Only meaningful in
    /// stage 1.
    pub fn picker_on_add_row(&self) -> bool {
        self.picker_provider.is_none() && self.modal_index == self.providers_filtered().len()
    }

    /// Whether `modal_index` is on the stage-2 "＋ Add model" row — the synthetic
    /// trailing row present only for a drilled-into custom provider.
    pub fn picker_on_add_model_row(&self) -> bool {
        self.picker_provider.is_some()
            && self.picker_provider_is_custom()
            && self.modal_index == self.provider_models_filtered().len()
    }

    /// Open the add-model overlay for custom provider `id`. The borrowed input
    /// line is a filter; `↑/↓` move the highlight over the matching candidates.
    pub fn open_add_model_overlay(&mut self, id: String) {
        self.add_model_provider = Some(id);
        self.add_model_choice = 0;
        self.input.clear();
        self.set_cursor(0);
        self.active_modal = Modal::AddModel;
    }

    /// The add-model overlay's suggestions matching the live filter: the
    /// provider's protocol candidates that fuzzy-match, plus the raw typed text
    /// as a custom id when it is not already a candidate.
    pub fn add_model_suggestions(&self) -> Vec<String> {
        let q = self.input.trim();
        let mut out: Vec<String> = self
            .add_model_candidates()
            .into_iter()
            .filter(|id| {
                q.is_empty()
                    || id.contains(q)
                    || crate::tui::fuzzy::fuzzy_match(&crate::tui::model_display_name(id), q)
                        .is_some()
            })
            .map(|s| s.to_string())
            .collect();
        if !q.is_empty() && !out.iter().any(|m| m == q) {
            out.push(q.to_string());
        }
        out
    }

    /// Move the add-model highlight over the filtered suggestions.
    pub fn move_add_model(&mut self, forward: bool) {
        let len = self.add_model_suggestions().len();
        if len == 0 {
            return;
        }
        self.add_model_choice = if forward {
            (self.add_model_choice + 1) % len
        } else {
            (self.add_model_choice + len - 1) % len
        };
    }

    /// Reset the add-model highlight after the filter query changes.
    pub fn on_add_model_filter_changed(&mut self) {
        self.add_model_choice = 0;
    }

    /// The model id the add-model overlay would submit: the highlighted
    /// suggestion. Empty when there are no matches.
    pub fn add_model_selected(&self) -> String {
        let suggestions = self.add_model_suggestions();
        suggestions
            .get(self.add_model_choice)
            .cloned()
            .unwrap_or_default()
    }

    /// Number of selectable rows in the Tools modal — the tool list, the
    /// only interactive surface. Used to clamp the Up/Down selection cursor.
    pub fn session_tools_len(&self) -> usize {
        self.session_context
            .as_ref()
            .map(|s| s.tools.len())
            .unwrap_or(0)
    }

    /// Build the mutation request implied by toggling the selected tool in the
    /// Tools modal, or `None` when there is no snapshot or the selection
    /// is out of range. The harness applies it and replies with a fresh
    /// snapshot that re-renders the modal.
    pub fn session_activate_request(&self) -> Option<AgentRequest> {
        let tool = self.session_context.as_ref()?.tools.get(self.modal_index)?;
        Some(AgentRequest::ToggleTool {
            name: tool.name.clone(),
            enabled: !tool.enabled,
        })
    }
}
