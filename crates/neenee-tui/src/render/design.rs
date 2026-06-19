//! Non-color design tokens: spacing, gutters, fixed row counts, and text
//! measurement limits shared by renderer components.

/// Uniform horizontal inset applied to transcript-area components so bands,
/// bars, and text do not touch the terminal frame.
pub(super) const TRANSCRIPT_H_INSET: u16 = 2;

/// Extra leading whitespace applied to prose after the transcript-area gutter.
pub(super) const TRANSCRIPT_BODY_LEADING_INDENT: u16 = 2;
/// Left prefix used by prose-like content: transcript gutter + body indent.
pub(super) const TRANSCRIPT_BODY_PREFIX_COLS: u16 =
    TRANSCRIPT_H_INSET + TRANSCRIPT_BODY_LEADING_INDENT;
/// Right-side slack reserved when wrapping prose-like content.
pub(super) const TRANSCRIPT_BODY_RIGHT_INSET: u16 = TRANSCRIPT_H_INSET;

/// Minimum readable width for compact expandable step header rows.
pub(super) const STEP_MIN_WIDTH: usize = 8;

/// One blank row inserted between transcript items unless a component already
/// provides its own separator.
pub(super) const MESSAGE_GAP_ROWS: usize = 1;

/// Vertical chrome rows around a sent user message panel: one top transition
/// row and one bottom transition row.
pub(super) const USER_MESSAGE_TRANSITION_ROWS: usize = 1;

/// Breathing room inside expanded tool steps.
pub(super) const TOOL_STEP_BODY_TOP_GAP_ROWS: usize = 1;
pub(super) const TOOL_STEP_SECTION_GAP_ROWS: usize = 1;
pub(super) const TOOL_STEP_CHILDREN_GAP_ROWS: usize = TOOL_STEP_SECTION_GAP_ROWS;
pub(super) const TOOL_STEP_BODY_BOTTOM_GAP_ROWS: usize = 1;

/// Breathing room inside expanded reasoning traces. These stay independent
/// from tool-step spacing because reasoning is prose-like, not a panel.
/// There is no bottom-gap token: the message-level separator
/// (`MESSAGE_GAP_ROWS`) already supplies the single blank row between a trace
/// and the next component, so an extra trailing gap would double it.
pub(super) const REASONING_TRACE_BODY_TOP_GAP_ROWS: usize = 1;
pub(super) const REASONING_TRACE_BLOCK_GAP_ROWS: usize = 1;

/// Hint bar: a single-line status strip pinned directly below the input box
/// that surfaces workspace + model + goal + MCP + context-usage info that the
/// old top header used to carry. Always one row tall when visible (hidden only
/// while an overlay modal replaces the chrome).
pub(super) const HINT_BAR_ROWS: u16 = 1;
/// Internal left indent of hint-bar content, matching the composer's prompt
/// prefix feel.
pub(super) const HINT_BAR_INNER_PADDING: usize = 1;
/// Gap between the cwd and the right-aligned cluster (model/goal/MCP/ctx).
pub(super) const HINT_BAR_GAP_MIN: usize = 2;
/// Gap between adjacent right-aligned hint segments.
pub(super) const HINT_BAR_SEGMENT_GAP: usize = 2;
/// Upper bound on the displayed goal objective excerpt shown in the hint bar.
pub(super) const HINT_BAR_GOAL_MAX_CHARS: usize = 28;
/// Upper bound on the displayed cwd. When the working directory is deeper than
/// this, the leading path components collapse to `…` so the leaf and the
/// right-side cluster both stay visible.
pub(super) const HINT_BAR_PATH_MAX_CHARS: usize = 32;
/// Fill-cell count of the context-usage bar (`[██░░░░░░░░]`).
pub(super) const CONTEXT_USAGE_BAR_CELLS: usize = 10;

pub(super) const STATUS_BAR_ROWS: u16 = 1;
pub(super) const SUBAGENT_BAR_ROWS: u16 = 1;

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
/// trailing fill.
pub(super) const USER_MESSAGE_OUTER_GUTTER_COLS: usize = TRANSCRIPT_H_INSET as usize;
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
