//! Non-color design tokens: spacing, gutters, fixed row counts, and text
//! measurement limits shared by renderer components.

/// Uniform horizontal inset applied to chat-area components so bands, bars,
/// and text do not touch the terminal frame.
pub(super) const CHAT_H_INSET: u16 = 2;

/// Extra leading whitespace applied to prose after the chat-area gutter.
pub(super) const CHAT_BODY_LEADING_INDENT: u16 = 2;
/// Left prefix used by prose-like content: chat gutter + body indent.
pub(super) const CHAT_BODY_PREFIX_COLS: u16 = CHAT_H_INSET + CHAT_BODY_LEADING_INDENT;
/// Right-side slack reserved when wrapping prose-like content.
pub(super) const CHAT_BODY_RIGHT_INSET: u16 = CHAT_H_INSET;

/// Minimum readable width for compact expandable card bands.
pub(super) const CARD_MIN_WIDTH: usize = 8;

/// One blank row inserted between transcript items unless a component already
/// provides its own separator.
pub(super) const MESSAGE_GAP_ROWS: usize = 1;

/// Vertical chrome rows around a sent user message panel: one top transition
/// row and one bottom transition row.
pub(super) const USER_MESSAGE_TRANSITION_ROWS: usize = 1;

/// Breathing room inside expanded tool-step cards.
pub(super) const TOOL_CARD_BODY_TOP_GAP_ROWS: usize = 1;
pub(super) const TOOL_CARD_SECTION_GAP_ROWS: usize = 1;
pub(super) const TOOL_CARD_CHILDREN_GAP_ROWS: usize = TOOL_CARD_SECTION_GAP_ROWS;
pub(super) const TOOL_CARD_BODY_BOTTOM_GAP_ROWS: usize = 1;

/// Breathing room inside expanded reasoning traces. These stay independent
/// from tool-card spacing because reasoning is prose-like, not a panel.
pub(super) const REASONING_TRACE_BODY_TOP_GAP_ROWS: usize = 1;
pub(super) const REASONING_TRACE_BLOCK_GAP_ROWS: usize = 1;
pub(super) const REASONING_TRACE_BODY_BOTTOM_GAP_ROWS: usize = 1;

/// Header is a floating half-block panel: a top transition row, one or more
/// content rows, and a bottom transition row. The panel grows by one row when
/// a goal-checklist dock is shown. Hidden entirely when an overlay modal is
/// open (chrome_hidden).
pub(super) const HEADER_ROWS: u16 = 3;
pub(super) const HEADER_WITH_CHECKLIST_ROWS: u16 = 4;
/// Internal left indent of header content inside the panel (after the side
/// gutter), matching the composer's prompt prefix feel.
pub(super) const HEADER_PANEL_INNER_PADDING: usize = 2;
pub(super) const HEADER_GOAL_GAP: usize = 3;
pub(super) const HEADER_RIGHT_GAP_MIN: usize = 1;
pub(super) const HEADER_GOAL_MAX_CHARS: usize = 32;
/// Upper bound on the displayed cwd. When the working directory is deeper than
/// this, the leading path components collapse to `…` so the leaf and the
/// right-side cluster (model name, context bar) both stay visible.
pub(super) const HEADER_PATH_MAX_CHARS: usize = 40;
/// Minimum chat-column width at which the context-usage bar is drawn.
pub(super) const HEADER_CONTEXT_MIN_WIDTH: usize = 40;
/// Fill-cell count of the context-usage bar (`[██░░░░░░░░]`).
pub(super) const CONTEXT_USAGE_BAR_CELLS: usize = 10;

pub(super) const STATUS_BAR_ROWS: u16 = 1;
pub(super) const HINT_LINE_ROWS: u16 = 1;
pub(super) const SUBAGENT_BAR_ROWS: u16 = 1;

/// Horizontal inset applied to the footer area containing status/composer/hints.
pub(super) const FOOTER_H_INSET: u16 = CHAT_H_INSET;

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
/// trailing fill.
pub(super) const USER_MESSAGE_OUTER_GUTTER_COLS: usize = CHAT_H_INSET as usize;
pub(super) const USER_MESSAGE_TEXT_GAP_COLS: usize = 1;
