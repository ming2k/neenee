//! The live editable prompt box at the bottom of the screen: half-block
//! transitions, in-box text wrapping with vertical scroll to keep the caret
//! visible, and per-row layout-map recording for semantic selection / copy.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::layout::{BlockRegion, LayoutMap};

use super::text::{padded_tail, wrap_text};
use super::Theme;

/// Special message_idx for the live input box in the layout map, so semantic
/// selection / copy works on input text just like chat messages.
pub const INPUT_MSG_IDX: usize = usize::MAX - 2;

/// Draw the bordered input box at the bottom of the screen (opencode-style).
#[allow(clippy::too_many_arguments)]
pub fn draw_input(
    frame: &mut Frame,
    input_rect: Rect,
    input: &str,
    cursor_display_x: u16,
    accent: Color,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    record: bool,
    input_scroll: &mut usize,
) {
    // The input box is rendered manually (not via a ratatui Block) so the `┃`
    // bar can be half-height on the transition rows, matching the
    // sent-user-message treatment.
    let panel_bg = theme.panel_bg;
    let app_bg = theme.app_bg;
    let full_w = input_rect.width as usize;
    let content_w = full_w.saturating_sub(1); // minus the bar column
    let text_width = content_w.saturating_sub(1).max(1); // minus leading space
    let wrapped = wrap_text(input, text_width);

    let bar =
        |ch: &str, bg: Color| Span::styled(ch.to_string(), Style::default().bg(bg).fg(accent));

    // Number of text rows that fit inside the box (top/bottom transition rows
    // consume two lines). The box is sized by draw_chat to fit the wrapped text
    // up to half the terminal height, so when the text exceeds this height we
    // scroll to keep the cursor visible.
    let visible_rows = (input_rect.height as usize).saturating_sub(2).max(1);

    // Map the caret's display offset onto the wrapped grid.
    let cursor_x_u = cursor_display_x as usize;
    let mut cursor_line = wrapped.len().saturating_sub(1);
    let mut cursor_col = cursor_x_u;
    let mut acc = 0usize;
    for (i, wl) in wrapped.iter().enumerate() {
        let w = wl.text.width();
        if cursor_x_u <= acc + w {
            cursor_line = i;
            cursor_col = cursor_x_u.saturating_sub(acc);
            break;
        }
        acc += w;
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

    // Top transition: ▄ (lower-half block) bar + ▀ panel so only the bottom
    // half carries panel_bg, creating a half-row inset above the text. Block
    // elements guarantee a pixel-accurate 50% split (box-drawing ╻ is
    // font-dependent and often exceeds half height).
    lines.push(Line::from(vec![
        bar("▄", app_bg),
        Span::styled(
            "▀".repeat(content_w),
            Style::default().fg(app_bg).bg(panel_bg),
        ),
    ]));

    // Text rows: full-height █ + leading space + text, padded to full width.
    // Only the visible slice of wrapped lines is rendered so overflowing content
    // can scroll while the box stays within its terminal-sized bounds.
    if wrapped.is_empty() {
        lines.push(Line::from(vec![
            bar("█", panel_bg),
            Span::styled(" ".repeat(content_w), Style::default().bg(panel_bg)),
        ]));
    } else {
        let start = scroll;
        let end = (scroll + visible_rows).min(wrapped.len());
        for wl in &wrapped[start..end] {
            let used = 1 + wl.text.width(); // leading space + text
            lines.push(Line::from(vec![
                bar("█", panel_bg),
                Span::styled(" ", Style::default().bg(panel_bg)),
                Span::styled(
                    wl.text.clone(),
                    Style::default().bg(panel_bg).fg(theme.text),
                ),
                Span::styled(padded_tail(content_w, used), Style::default().bg(panel_bg)),
            ]));
        }
    }

    // Bottom transition: ▀ (upper-half block) bar + ▄ panel so only the top
    // half carries panel_bg.
    lines.push(Line::from(vec![
        bar("▀", app_bg),
        Span::styled(
            "▄".repeat(content_w),
            Style::default().fg(app_bg).bg(panel_bg),
        ),
    ]));

    frame.render_widget(Paragraph::new(lines), input_rect);

    // Record each visible text row in the layout map so mouse drag selection
    // and copy work on the live input. Skipped when the API-key modal masks
    // the display (byte offsets wouldn't match the real input).
    if record {
        let start = scroll;
        let end = (scroll + visible_rows).min(wrapped.len());
        for (i, wl) in wrapped[start..end].iter().enumerate() {
            let row_y = input_rect.y + 1 + i as u16;
            layout_map.push(BlockRegion {
                message_idx: INPUT_MSG_IDX,
                block_idx: 0,
                start_byte: wl.start_byte,
                end_byte: wl.end_byte,
                text: wl.text.clone(),
                prefix_cols: 1,
                rect: Rect::new(input_rect.x + 1, row_y, content_w as u16, 1),
            });
        }
    }

    // Position the caret relative to the visible slice.
    let visible_cursor_line = cursor_line.saturating_sub(scroll);
    let cursor_y = input_rect.y + 1 + visible_cursor_line as u16;
    let cursor_x = input_rect.x + 2 + cursor_col.min(text_width) as u16;
    frame.set_cursor(cursor_x, cursor_y);
}
