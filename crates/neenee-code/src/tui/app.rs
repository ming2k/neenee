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
    ProviderPickerSnapshot, Pursuit, Role, SessionOverview, TodoList, mcp::McpConnectionStatus,
};

use crate::tui::completion::PathScan;
use crate::tui::composer_attachments;
use crate::tui::document::{DeliveryStatus, TranscriptMessage};
use crate::tui::event_loop::resolve_focused_mut;
use crate::tui::fuzzy;
use crate::tui::layout::{InteractiveTarget, LayoutMap, ModalHitMap};
use crate::tui::providers::{PROVIDERS, RankedModel, models_filtered_from};
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
    /// Flat model picker (`Ctrl+P` / `/model`). A single searchable list of
    /// every `(provider, model)` pair — multi-model providers fan out into one
    /// row per model — that mirrors the input-history modal's two-mode design:
    /// it opens in **browse** mode (a plain ranked list, composer line not
    /// borrowed, typing inert) and `/` drops into a **search** sub-layer that
    /// borrows the line as a live fuzzy query (`App::model_search` distinguishes
    /// the two). Rows come from [`App::models_filtered`]; Enter activates the
    /// highlighted model. The first Esc in search returns to browse, the second
    /// (or an outside click) closes and restores the draft.
    Provider,
    /// Input-history recall (Ctrl+R). A two-mode surface: it opens in **browse**
    /// mode — a plain reverse-chronological list (newest first, top-focused)
    /// where the composer line is not borrowed and typing is inert — and `/`
    /// drops into a **search** sub-layer that borrows the line as a live fuzzy
    /// query (`App::history_search` distinguishes the two). The name is kept for
    /// continuity even though browsing, not searching, is now the default.
    /// Rows come from [`App::history_rows`]; Enter inserts the focused entry into
    /// the composer for editing (never sends). The first Esc in search returns to
    /// browse, the second (or an outside click) closes and restores the draft.
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
    /// Session context modal: a single scrollable **read-only** dashboard of
    /// the live session's model, MCP servers, and skills. Opened with the
    /// `/session` slash command. The dashboard's `TOOLS` line is a one-row
    /// summary (count) whose `t`/Enter action hands off to [`Modal::Tools`] for
    /// the interactive toggle surface — tools no longer live inline here.
    Session,
    /// Tools manager modal: a centered, dismissable, selectable list of every
    /// session tool — builtins, `mcp:<server>`, `pursuit`, `plan` — each with a
    /// `Space` toggle to enable/disable it. Opened with the `/tools` slash
    /// command (and via `t`/Enter from the session dashboard's TOOLS line).
    /// [`App::modal_index`] is its selection cursor; data comes from the same
    /// session-context snapshot `/session` uses.
    Tools,
    /// MCP manager modal: a centered, dismissable, selectable list of every
    /// configured MCP server with its connection status (connected / disabled /
    /// failed) and tool count. Opened with the `/mcp` slash command. `Space`
    /// toggles a server on/off for the session (connect/disconnect, applied
    /// live without rewriting config.toml); `r` reconnects the selected server.
    /// [`App::modal_index`] is its selection cursor; data comes from the same
    /// session-context snapshot `/session` uses (its `mcp` pane).
    Mcp,
    /// Permissions manager modal: a centered, dismissable overlay listing the
    /// session's cached "always allow" rules with per-row revoke and a
    /// clear-all action. Opened with the `/permissions` slash command. This
    /// is the management surface — distinct from [`Modal::Permission`] (the
    /// inline real-time approval sheet).
    Permissions,
    /// Activity overview: the current pursuit (objective + checklist), the live
    /// plan-progress breakdown, and the running turn/round/model/elapsed/
    /// status. Opened by clicking the activity bar. The body scrolls via
    /// [`App::activity_scroll`].
    Activity,
}

/// How the live surface recedes while a modal owns the foreground.
///
/// A terminal cannot alpha-blend, so a modal expresses "the background has
/// receded" in one of three ways instead of painting a translucent veil. This
/// is the single source of truth that both the footer-collapse decision
/// ([`App`]/event loop) and the per-frame recess pass (`render::recess_backdrop`)
/// consult, so layout and paint can never disagree about what a modal does to
/// the surface beneath it.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Recess {
    /// The modal floats on the fully-live surface. No dimming, no occlusion —
    /// used by lightweight overlays that never take over (Question, Permission).
    None,
    /// The surface stays mounted and is darkened in place so the centered modal
    /// reads as the focal layer while context (transcript, input, hint bar,
    /// activity bar) remains visible. The brightness factor comes from
    /// [`Theme::modal_dim_factor`](crate::tui::render::Theme::modal_dim_factor).
    Dim,
    /// Full takeover: the footer collapses to zero height and the surface is
    /// occluded with a solid fill. Reserved for context-switching flows
    /// (session selection) where a clean slate is the intent.
    Takeover,
}

impl Modal {
    /// The recess policy for this modal — the single source of truth that the
    /// footer-collapse flag and the per-frame recess pass both key off.
    pub fn recess(self) -> Recess {
        match self {
            // Float: lightweight overlays that never touch the surface.
            Modal::None | Modal::Question | Modal::Permission => Recess::None,
            // Context switch: the one modal that fully owns the screen.
            Modal::Sessions => Recess::Takeover,
            // Everything else recedes the surface for focus while keeping it
            // visible (transcript, chrome, and all).
            _ => Recess::Dim,
        }
    }

    /// Whether this modal closes when the user clicks outside its rect
    /// (click-outside-to-dismiss). True for the read-only / info overlays
    /// (Help, Session, Sessions, Activity) and for the history
    /// modal and the model picker: their filter query is ephemeral and the real
    /// composer draft is safely parked in `stashed_input`, so an outside click
    /// closes them and restores the draft (via [`App::restore_history_draft`]) —
    /// exactly like Esc. Entry modals that hold precious in-progress input
    /// (ModelEditor, Question) and the permission sheet stay open so an
    /// accidental click never discards an API key or a pending decision.
    ///
    /// This is the single source of truth for *which* modals are
    /// click-dismissable; the event loop records the renderer's actual panel
    /// rect for these modals and leaves every other modal without an
    /// outside-click target.
    pub fn dismissable_by_outside_click(self) -> bool {
        matches!(
            self,
            Modal::Help
                | Modal::Session
                | Modal::Tools
                | Modal::Mcp
                | Modal::Sessions
                | Modal::Permissions
                | Modal::Activity
                | Modal::HistorySearch
                | Modal::Provider
        )
    }
}

/// Which section the Activity modal is showing. Each section is opened
/// independently by clicking the corresponding segment on the activity bar,
/// so there is no tab strip or Left/Right cycling — the variant simply
/// controls which content the modal body renders.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ActivityTab {
    Activity,
    Todos,
}

impl ActivityTab {
    /// Modal title shown in the header.
    pub fn title(self) -> &'static str {
        match self {
            ActivityTab::Activity => "Activity",
            ActivityTab::Todos => "Todos",
        }
    }
}

/// Capturable snapshot of the main transcript's scroll position, saved when
/// zooming into a nested view and restored on return.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct ScrollSnapshot {
    pub offset: u16,
    pub follow_bottom: bool,
}

/// One frame on the focus stack: the subagent task call-id plus the parent
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
    /// streaming, subagent view, or chrome hidden).
    pub activity_rect: Option<neenee_tui::Rect>,
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
    /// Stack of nested zoom frames (subagent tasks). Empty means the root
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
    /// Body scroll offset of the session-context dashboard ([`Modal::Session`]).
    /// Reset to 0 on open. Clamped (and, when `session_modal_follow` is set,
    /// auto-followed to the tool selection) by the renderer each frame.
    pub session_scroll: usize,
    /// When true, the session dashboard's body scroll follows the ↑/↓ tool
    /// selection cursor (the default after open / navigation). Cleared the
    /// moment the user scrolls manually (wheel / page keys) so they can browse
    /// freely, and re-set the moment they navigate again.
    pub session_modal_follow: bool,
    /// Body scroll offset of the permissions manager modal. Reset to 0 each
    /// time the modal opens; clamped and auto-followed to the selection by the
    /// renderer each frame.
    pub permissions_scroll: usize,
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
    /// Latest session-context snapshot for the session modal, or `None` before
    /// the first `QuerySessionContext` round-trip completes. Refreshed each
    /// frame from the response listener.
    pub session_context: Option<neenee_core::SessionContextSnapshot>,
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
    pub turn_count: u64,
    /// Current tool round within the active turn (1-indexed for display:
    /// `0` means the turn has started but no model request has fired yet —
    /// e.g. the "queued" / "preparing context" phase). Mirrored each frame
    /// from the response listener; shown in the Activity modal as
    /// `turn N · round M · <status>`.
    pub current_round: u64,
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
    /// Keyboard-focused activatable target in the current frame, and the TUI's
    /// only navigation state — there is no separate "browse mode". `None` means
    /// every key has its ordinary input-box meaning (typing flows into the
    /// prompt). `Some` means a transcript step is highlighted: `Ctrl+↑`/`Ctrl+↓`
    /// (or bare `↑`/`↓`) cycle it, `Enter` activates it, and `Esc` clears it.
    /// Mouse hover/click is an acceleration path onto the same state.
    pub focused_target: Option<InteractiveTarget>,
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
    /// Epoch the breathing indicator is timed against. The spinner phase is
    /// derived from wall-clock elapsed time since this instant rather than a
    /// per-frame counter, so the breathing cadence stays constant regardless of
    /// how often the loop redraws (mouse movement, streaming, paste, etc. all
    /// wake the loop at irregular intervals and would otherwise jitter it).
    pub spinner_epoch: std::time::Instant,
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
    /// Whether the model picker's **search sub-layer** is active. The picker
    /// ([`Modal::Provider`]) opens in browse mode (`false`): a plain ranked list
    /// with no query field. Pressing `/` enters search (`true`), which borrows
    /// the composer line as a live fuzzy query; the first Esc returns to browse
    /// and the second closes the modal. Mirrors [`Self::history_search`]. See
    /// [`Self::models_filtered`].
    pub model_search: bool,
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
        self.cursor_position = self.input[..cursor_byte].chars().count();
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

    /// Whether the view is currently zoomed into a subagent task.
    pub fn in_subagent_view(&self) -> bool {
        !self.focus_stack.is_empty()
    }

    /// The message slice currently in view: the `/btw` side transcript when
    /// the side view is active (ADR-0017), the focused subagent task's child
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
                if message.is_subagent_task()
                    && message.tool_step_call_id() == Some(frame.call_id.as_str())
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

    /// Zoom into a subagent task's child messages.
    pub fn enter_subagent(&mut self, call_id: String) {
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

    /// Return from the current subagent view to its parent. Returns true if a
    /// view was actually popped.
    pub fn exit_subagent(&mut self) -> bool {
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
    /// reports the primary session's coarse status. Reuses the subagent
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

    /// Cycle to the previous (`dir < 0`) or next (`dir > 0`) sibling subagent
    /// task at the current focus level. No-op when not in a subagent view or
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
                if message.is_subagent_task() {
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

    /// Tear down the history modal's borrowed state: hand the parked composer
    /// draft back, drop any filter query, and clear the search/preview
    /// sub-flags. Shared by the Esc (`CloseModal`) and click-outside dismiss
    /// paths so the two can never drift. Does **not** touch `active_modal` —
    /// the caller owns that transition.
    pub fn restore_history_draft(&mut self) {
        self.input = std::mem::take(&mut self.stashed_input);
        self.cursor_position = self.input.chars().count();
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
        self.cursor_position = self.input.chars().count();
        self.input_scroll = 0;
        self.suggestion_index = None;
        self.modal_index = 0;
        self.model_search = false;
        self.model_scroll = 0;
        self.model_modal_follow = true;
    }

    /// Compute the flat, ranked model rows for the picker. Delegates to
    /// [`models_filtered_from`] so the input handler and the renderer share one
    /// filter+sort implementation. The query is the borrowed composer line only
    /// while the search sub-layer is active; in browse mode it is empty so every
    /// row is shown unhighlighted.
    pub fn models_filtered(&self) -> Vec<RankedModel> {
        let query = if self.model_search {
            self.input.trim()
        } else {
            ""
        };
        models_filtered_from(PROVIDERS, &self.provider_picker, query)
    }

    /// Number of selectable rows in the session dashboard — the tool list, the
    /// only interactive surface. Used to clamp the Up/Down selection cursor.
    pub fn session_tools_len(&self) -> usize {
        self.session_context
            .as_ref()
            .map(|s| s.tools.len())
            .unwrap_or(0)
    }

    /// Build the mutation request implied by toggling the selected tool in the
    /// session dashboard, or `None` when there is no snapshot or the selection
    /// is out of range. The harness applies it and replies with a fresh
    /// snapshot that re-renders the dashboard.
    pub fn session_activate_request(&self) -> Option<AgentRequest> {
        let tool = self.session_context.as_ref()?.tools.get(self.modal_index)?;
        Some(AgentRequest::ToggleTool {
            name: tool.name.clone(),
            enabled: !tool.enabled,
        })
    }
}
