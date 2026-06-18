//! The live editable prompt box at the bottom of the screen: in-box text
//! wrapping with vertical scroll to keep the caret visible, and per-row
//! layout-map recording for semantic selection / copy.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::layout::{BlockRegion, LayoutMap};

use super::design::{
    COMPOSER_PROMPT_PREFIX_COLS, COMPOSER_TEXT_ROW_OFFSET, COMPOSER_VERTICAL_CHROME_ROWS,
};
use super::text_layout::{padded_tail, wrap_text, WrappedLine};
use super::Theme;

/// Special message_idx for the live input box in the layout map, so semantic
/// selection / copy works on input text just like chat messages.
pub const INPUT_MSG_IDX: usize = usize::MAX - 2;

/// Build the wrapped-line list the composer renders, including the synthetic
/// trailing row it appends when the caret rests past the last wrapped line
/// (e.g. just after an inserted newline). Both the height computation in
/// [`super::draw_chat`] and the actual rendering in [`draw_composer`] go
/// through this so the box never scrolls its own prompt glyph out of view on
/// the first newline.
fn composer_wrapped(input: &str, text_width: usize, byte_cursor: usize) -> Vec<WrappedLine> {
    let mut wrapped = wrap_text(input, text_width);
    let last_end = wrapped.last().map_or(0, |w| w.end_byte);
    if byte_cursor > last_end {
        wrapped.push(WrappedLine {
            text: String::new(),
            start_byte: last_end,
            end_byte: byte_cursor.max(last_end),
        });
    }
    wrapped
}

/// Number of text rows the composer will render for `input`, accounting for
/// the trailing caret row. Always at least 1 so an empty box still reserves a
/// prompt line.
pub(super) fn input_row_count(input: &str, text_width: usize, byte_cursor: usize) -> usize {
    composer_wrapped(input, text_width, byte_cursor).len().max(1)
}

/// Draw the flat input box panel at the bottom of the screen.
#[allow(clippy::too_many_arguments)]
pub fn draw_composer(
    frame: &mut Frame,
    input_rect: Rect,
    input: &str,
    byte_cursor: usize,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    record: bool,
    input_scroll: &mut usize,
) {
    // The input box is a flat panel: each text row carries panel_bg and is
    // prefixed with `› ` on the first wrapped line / a two-space indent on
    // continuations. The top and bottom edges are half-block rows so the panel
    // floats a half row off the app background.
    let panel_bg = theme.input_bg;
    let app_bg = theme.app_bg;
    let full_w = input_rect.width as usize;
    let text_width = full_w.saturating_sub(COMPOSER_PROMPT_PREFIX_COLS).max(1);
    let mut wrapped = composer_wrapped(input, text_width, byte_cursor);

    // Number of text rows that fit inside the box (top/bottom transition rows
    // consume two lines). The box is sized by draw_chat to fit the wrapped text
    // up to half the terminal height, so when the text exceeds this height we
    // scroll to keep the cursor visible.
    let visible_rows = (input_rect.height as usize)
        .saturating_sub(COMPOSER_VERTICAL_CHROME_ROWS as usize)
        .max(1);

    // Map the caret's byte offset onto the wrapped grid. Each WrappedLine
    // records its byte range in the original input, so this stays correct for
    // explicit newlines (whose display width is 0 and would otherwise collapse
    // a multi-line caret onto the first row).
    let mut cursor_line = wrapped.len().saturating_sub(1);
    let mut cursor_col = 0usize;
    for (i, wl) in wrapped.iter().enumerate() {
        if byte_cursor <= wl.end_byte {
            cursor_line = i;
            let local = byte_cursor.saturating_sub(wl.start_byte).min(wl.text.len());
            cursor_col = wl.text[..local].width();
            break;
        }
    }

    // Keep the cursor line inside the visible window. Clamp to the valid scroll
    // range so the input box never shows empty padding below the text.
    let max_scroll = wrapped.len().saturating_sub(visible_rows);
    let mut scroll = *input_scroll;
    if wrapped.len() <= visible_rows {
        scroll = 0;
    } else {
        if cursor_line < scroll {
            scroll = cursor_line;
        } else if cursor_line >= scroll + visible_rows {
            scroll = cursor_line.saturating_sub(visible_rows - 1);
        }
        scroll = scroll.min(max_scroll);
    }
    *input_scroll = scroll;

    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows + 2);
    let top_edge = Span::styled("▄".repeat(full_w), Style::default().fg(panel_bg).bg(app_bg));
    let bottom_edge = Span::styled("▀".repeat(full_w), Style::default().fg(panel_bg).bg(app_bg));

    // Top edge: lower-half blocks so only the bottom half of the row carries
    // panel_bg, meeting the full-height text rows below it.
    lines.push(Line::from(top_edge.clone()));

    // Text rows: `› ` marks the first logical line and a two-space indent
    // marks every wrapped continuation, so the box reads as a shell-style
    // prompt. Only the visible slice is rendered so overflowing content can
    // scroll while the box stays within its terminal-sized bounds.
    let prompt_glyph = Span::styled("›", Style::default().bg(panel_bg).fg(theme.accent));
    let prompt_gap = Span::styled(" ", Style::default().bg(panel_bg));
    let indent = Span::styled(
        " ".repeat(COMPOSER_PROMPT_PREFIX_COLS),
        Style::default().bg(panel_bg),
    );
    if wrapped.is_empty() {
        let used = COMPOSER_PROMPT_PREFIX_COLS;
        lines.push(Line::from(vec![
            prompt_glyph.clone(),
            prompt_gap.clone(),
            Span::styled(padded_tail(full_w, used), Style::default().bg(panel_bg)),
        ]));
    } else {
        let start = scroll;
        let end = (scroll + visible_rows).min(wrapped.len());
        for (i, wl) in wrapped[start..end].iter().enumerate() {
            let used = COMPOSER_PROMPT_PREFIX_COLS + wl.text.width();
            let mut spans = if start + i == 0 {
                vec![prompt_glyph.clone(), prompt_gap.clone()]
            } else {
                vec![indent.clone()]
            };
            spans.push(Span::styled(
                wl.text.clone(),
                Style::default().bg(panel_bg).fg(theme.text),
            ));
            spans.push(Span::styled(
                padded_tail(full_w, used),
                Style::default().bg(panel_bg),
            ));
            lines.push(Line::from(spans));
        }
    }

    // Bottom edge: upper-half blocks so only the top half of the row carries
    // panel_bg, meeting the full-height text rows above it.
    lines.push(Line::from(bottom_edge));

    frame.render_widget(Paragraph::new(lines), input_rect);

    // Record each visible text row in the layout map so mouse drag selection
    // and copy work on the live input. Skipped when the API-key modal masks
    // the display (byte offsets wouldn't match the real input).
    if record {
        let start = scroll;
        let end = (scroll + visible_rows).min(wrapped.len());
        for (i, wl) in wrapped[start..end].iter().enumerate() {
            let row_y = input_rect.y + COMPOSER_TEXT_ROW_OFFSET + i as u16;
            layout_map.push(BlockRegion {
                message_idx: INPUT_MSG_IDX,
                block_idx: 0,
                start_byte: wl.start_byte,
                end_byte: wl.end_byte,
                text: wl.text.clone(),
                prefix_cols: COMPOSER_PROMPT_PREFIX_COLS as u16,
                rect: Rect::new(input_rect.x, row_y, full_w as u16, 1),
            });
        }
    }

    // Position the caret relative to the visible slice, after the `> ` /
    // indent prefix.
    let visible_cursor_line = cursor_line.saturating_sub(scroll);
    let cursor_y = input_rect.y + COMPOSER_TEXT_ROW_OFFSET + visible_cursor_line as u16;
    let cursor_x =
        input_rect.x + COMPOSER_PROMPT_PREFIX_COLS as u16 + cursor_col.min(text_width) as u16;
    frame.set_cursor(cursor_x, cursor_y);
}
