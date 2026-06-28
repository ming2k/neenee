//! Non-color design tokens: spacing, gutters, fixed row counts, and text
//! measurement limits shared by renderer components.

/// Uniform horizontal inset applied to transcript-area components so bands,
/// bars, and text do not touch the terminal frame.
pub(super) const TRANSCRIPT_H_INSET: u16 = 2;

/// Extra leading whitespace applied to prose after the transcript-area gutter.
/// Now that the horizontal gutter is applied once at the stream entry point,
/// this is the *only* indent prose-like content adds on top of the already-
/// inset rect.
pub(super) const TRANSCRIPT_BODY_LEADING_INDENT: u16 = 2;

/// Minimum readable width for compact expandable step header rows.
pub(super) const STEP_MIN_WIDTH: usize = 8;

/// One blank row inserted between transcript items unless a component already
/// provides its own separator.
pub(super) const MESSAGE_GAP_ROWS: usize = 1;

/// Vertical chrome rows around a sent user message panel: one top transition
/// row and one bottom transition row.
pub(super) const USER_MESSAGE_TRANSITION_ROWS: usize = 1;

/// Breathing room around an expanded tool step's body. The gap belongs to the
/// *body*, not the step boundary: collapsed tool steps stack flush (no blank
/// row between adjacent headers — see `draw_transcript`'s
/// `collapsed_tool_into_tool_step` suppression), so a batch of parallel /
/// sequential tool calls reads as one compact log block. Only an expanded body
/// is padded — `TOOL_STEP_BODY_TOP_GAP_ROWS` rows above it (separating it from
/// its own header) and one `MESSAGE_GAP_ROWS` row below it (separating it from
/// the next step's header). There is no dedicated bottom-gap token: the
/// message-level separator supplies that single trailing row, so an extra
/// trailing gap here would double it.
pub(super) const TOOL_STEP_BODY_TOP_GAP_ROWS: usize = 1;
pub(super) const TOOL_STEP_SECTION_GAP_ROWS: usize = 1;
pub(super) const TOOL_STEP_CHILDREN_GAP_ROWS: usize = TOOL_STEP_SECTION_GAP_ROWS;

/// Breathing room inside expanded reasoning traces. These stay independent
/// from tool-step spacing because reasoning is prose-like, not a panel.
/// There is no bottom-gap token: the message-level separator
/// (`MESSAGE_GAP_ROWS`) already supplies the single blank row between a trace
/// and the next component, so an extra trailing gap would double it.
pub(super) const REASONING_TRACE_BODY_TOP_GAP_ROWS: usize = 1;
pub(super) const REASONING_TRACE_BLOCK_GAP_ROWS: usize = 1;

/// Hint bar: a single-line status strip pinned directly below the input box
/// that surfaces model + context-usage info. Always one row tall when visible
/// (hidden only while an overlay modal replaces the chrome).
pub(super) const HINT_BAR_ROWS: u16 = 1;
/// Internal left indent of hint-bar content, matching the composer's prompt
/// prefix feel.
pub(super) const HINT_BAR_INNER_PADDING: usize = 1;
/// Minimum gap between the left cluster (shell pill) and the
/// right-aligned cluster (model/context).
pub(super) const HINT_BAR_GAP_MIN: usize = 2;
/// Gap between adjacent right-aligned hint segments.
pub(super) const HINT_BAR_SEGMENT_GAP: usize = 2;

pub(super) const STATUS_BAR_ROWS: u16 = 1;
pub(super) const ENVOY_BAR_ROWS: u16 = 1;
/// Height of the `/btw` side banner (ADR-0017): a single line at the top of
/// the transcript viewport.
pub(super) const SIDE_BANNER_ROWS: u16 = 1;

/// Horizontal inset applied to the footer area containing status/composer/hints.
pub(super) const FOOTER_H_INSET: u16 = TRANSCRIPT_H_INSET;

/// Composer chrome consists of one top and one bottom padding row.
pub(super) const COMPOSER_VERTICAL_CHROME_ROWS: u16 = 2;
pub(super) const COMPOSER_MIN_HEIGHT: u16 = 3;
pub(super) const COMPOSER_MAX_HEIGHT_DIVISOR: u16 = 2;
/// Columns reserved before the composer text: a `>` prompt glyph plus a space
/// on the first wrapped line, matched by a two-space indent on every wrapped
/// continuation line so the caret stays aligned.
pub(super) const COMPOSER_PROMPT_PREFIX_COLS: usize = 2;
pub(super) const COMPOSER_TEXT_ROW_OFFSET: u16 = 1;

/// User message panels mirror the composer: outer gutter, gap, text, then
/// User message panels used to reserve their own outer gutter matching
/// [`TRANSCRIPT_H_INSET`]. Now that the horizontal inset is applied once at
/// the stream entry point (`draw_transcript` → `band`), the outer gutter is
/// redundant: the `band` rect already excludes it. Set to 0 so the panel
/// background starts at the band edge, with only the inner text gap remaining.
pub(super) const USER_MESSAGE_OUTER_GUTTER_COLS: usize = 0;
/// Inner left padding (in `user_panel_bg`) between the outer gutter and the
/// text. Matches the composer's prompt prefix so sent messages and the input
/// box share the same left margin.
pub(super) const USER_MESSAGE_TEXT_GAP_COLS: usize = 2;
/// Inner right padding (in `user_panel_bg`) kept clear of wrapped text so a
/// sent message never runs its text into the panel's right edge.
pub(super) const USER_MESSAGE_RIGHT_PAD_COLS: usize = 2;

/// Inner right padding (in `input_bg`) kept clear of wrapped text inside the
/// composer, mirroring the left prompt prefix so the box reads as a balanced
/// panel.
pub(super) const COMPOSER_RIGHT_PAD_COLS: usize = 2;

// ── Modal overlays ───────────────────────────────────────────────────────
// Every centered modal (Activity, Sessions, Provider, Help, …) goes through
// `modal_frame`, which paints a borderless solid-bg panel and splits it into
// header / body / footer. These tokens are the single source of truth for
// spacing *inside* that panel, so every modal indents its content the same
// way instead of hard-coding whitespace per file.

/// Left/right padding between the panel edge and the header/body/footer.
/// Applied once by `modal_frame` via `Margin { horizontal, .. }`; section
/// content never adds its own outer gutter on top of this. Includes room for
/// the scrollbar track (1 col) plus `SCROLLBAR_GAP` (1 col) on the right.
pub(super) const MODAL_INNER_H_PADDING: u16 = 3;

/// Empty columns between the body text's right edge and the scrollbar track.
pub(super) const SCROLLBAR_GAP: u16 = 1;

/// Top/bottom padding between the panel edge and the header/body/footer.
/// Applied once by `modal_frame` via `Margin { vertical, .. }`.
pub(super) const MODAL_INNER_V_PADDING: u16 = 1;

/// Leading indent for body content (items, prose) under the header or a
/// section label, so all sections align across every modal regardless of
/// which overlay renders them. Added on top of `MODAL_INNER_H_PADDING`.
pub(super) const MODAL_BODY_LEADING_INDENT: usize = 2;

/// Columns between a header title and a trailing meta value shown beside it
/// (e.g. the Todos `done/total` counter), so title + meta read as one line.
pub(super) const MODAL_TITLE_META_GAP: usize = 2;

// ── Left-bar panels (panel_block family) ─────────────────────────────────
// `panel_block` is a borderless solid-bg panel with a single thick colored
// left `┃` bar — the severity/identity cue shared by the tool-step detail
// overlay and the permission sheet. These tokens size the content rect
// inside it, the left-bar-panel family's counterpart to `modal_frame`'s
// `MODAL_INNER_H_PADDING` (which insets the borderless modal family).

/// Per-side horizontal inset of `panel_block` content: the thick left `┃`
/// bar occupies 1 column, and a matching 1-column gutter is reserved on the
/// right so the panel's content is symmetric and a long line never runs
/// into either edge. Applied as a symmetric margin by `panel_inner`. The
/// permission sheet deliberately layers its own `PERMISSION_H_PADDING` on
/// top for button breathing room, so it computes its own content rect.
pub(super) const PANEL_BAR_INSET: u16 = 1;
