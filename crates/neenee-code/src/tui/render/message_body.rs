//! Markdown block-level rendering for a single message: text, code, tables,
//! headings, quotes, lists, rules, breaks. Emits one rendered line per row
//! and records semantic [`BlockRegion`]s / table cell hit boxes for selection
//! and click hit-testing.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::document::{Block, DeliveryStatus, TranscriptMessage};
use crate::tui::layout::{BlockRegion, LayoutMap, TableCellHit};
use crate::tui::selection::SelectionState;

use super::design::{
    USER_MESSAGE_OUTER_GUTTER_COLS, USER_MESSAGE_RIGHT_PAD_COLS, USER_MESSAGE_TEXT_GAP_COLS,
    USER_MESSAGE_TRANSITION_ROWS,
};
use super::markdown_table::{build_table_render, push_table_segment};
use super::text_layout::{
    WrappedLine, block_selection_range, code_gutter_line, line_selection, line_spans_rich,
    padded_tail, wrap_text,
};
use super::{TRANSCRIPT_BODY_PREFIX_COLS, TRANSCRIPT_BODY_RIGHT_INSET, Theme};

fn display_width_u16(s: &str) -> u16 {
    s.width() as u16
}

/// Render the blocks of a single message inside the given area.
///
/// This is extracted so that normal messages and tool steps can share
/// the same block-rendering logic while using different containing rects.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_message_body(
    frame: &mut Frame,
    area: Rect,
    msg: &TranscriptMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    record_layout: bool,
) {
    for (bi, block) in msg.blocks.iter().enumerate() {
        let sel_range = block_selection_range(selection, mi, bi);

        // Gap before a list is handled structurally: the parser's `push_block`
        // already inserts a `Block::Break` at every list↔non-list boundary (and
        // only there), so the list reads as a discrete group with exactly one
        // blank line of separation. Adjacent list items never get a break, so
        // list entries stay tight — as in rendered markdown. Adding another
        // blank row here used to double that gap (two lines instead of one).

        match block {
            Block::Text {
                content,
                code_ranges,
            } => {
                let is_user = msg.role == neenee_core::Role::User;
                let is_queued = is_user && msg.delivery == DeliveryStatus::Queued;
                let base = match msg.role {
                    neenee_core::Role::User => Style::default().fg(theme.user_text()),
                    neenee_core::Role::System => Style::default().fg(theme.system_text()),
                    _ => Style::default().fg(theme.fg()),
                };
                let full_width = area.width as usize;
                let body_wrap_width = area
                    .width
                    .saturating_sub(TRANSCRIPT_BODY_PREFIX_COLS + TRANSCRIPT_BODY_RIGHT_INSET)
                    as usize;
                // User messages render inside their own panel, so they wrap at
                // the panel's inner width minus symmetric left/right padding
                // rather than the shared prose width — this keeps the text from
                // running into either edge of the `user_panel_bg` band.
                let user_panel_w = full_width.saturating_sub(2 * USER_MESSAGE_OUTER_GUTTER_COLS);
                let user_text_width = user_panel_w
                    .saturating_sub(USER_MESSAGE_TEXT_GAP_COLS + USER_MESSAGE_RIGHT_PAD_COLS)
                    .max(1);
                let lines = wrap_text(
                    content,
                    if is_user {
                        user_text_width
                    } else {
                        body_wrap_width
                    },
                );
                *content_lines += lines.len();

                // User messages get top/bottom padding rows (matching the input
                // box's breathing room).  The padding is a blank `user_panel_bg`
                // row with the `┃` bar so the message reads as a solid panel.
                // Queued messages swap in the dimmer `user_surface_queued` so a
                // pending send reads as more "pending" than delivered.
                let user_bg = if is_queued {
                    theme.user_surface_queued()
                } else {
                    theme.user_surface()
                };
                let user_gutter = " ".repeat(USER_MESSAGE_OUTER_GUTTER_COLS);
                let user_content_w = full_width.saturating_sub(2 * USER_MESSAGE_OUTER_GUTTER_COLS);

                if is_user {
                    for _ in 0..USER_MESSAGE_TRANSITION_ROWS {
                        *content_lines += 1;
                        if *skip_rows > 0 {
                            *skip_rows = skip_rows.saturating_sub(1);
                        } else if *current_y < area.y + area.height {
                            // Top edge: lower-half blocks so only the bottom half
                            // carries user_panel_bg, meeting the text rows below.
                            let pad = Line::from(vec![
                                Span::styled(
                                    user_gutter.clone(),
                                    Style::default().bg(theme.surface()),
                                ),
                                Span::styled(
                                    "▄".repeat(user_content_w),
                                    Style::default().fg(user_bg).bg(theme.surface()),
                                ),
                                Span::styled(
                                    user_gutter.clone(),
                                    Style::default().bg(theme.surface()),
                                ),
                            ]);
                            let rect = Rect::new(area.x, *current_y, area.width, 1);
                            frame.render_widget(Paragraph::new(pad), rect);
                            *current_y += 1;
                        }
                    }
                    // Queued badge: render a single-row "⏸ Queued" label inside
                    // the panel before the first text line, so the user can tell
                    // at a glance the message is staged in the send queue. Only
                    // the first text block carries the badge — multi-block
                    // markdown user messages are rare and the dimmer bg still
                    // conveys the state on the remaining blocks.
                    if is_queued && bi == 0 {
                        *content_lines += 1;
                        if *skip_rows > 0 {
                            *skip_rows = skip_rows.saturating_sub(1);
                        } else if *current_y < area.y + area.height {
                            let badge = "⏸ Queued";
                            let badge_w = badge.width();
                            // Reserve a 2-col gap after the badge so it does
                            // not run into any wrapped text on the same row
                            // (the badge row has no text — text starts on the
                            // next row — but the cushion reads cleaner).
                            let used = USER_MESSAGE_TEXT_GAP_COLS + badge_w;
                            let mut spans = vec![
                                Span::styled(
                                    user_gutter.clone(),
                                    Style::default().bg(theme.surface()),
                                ),
                                Span::styled(
                                    " ".repeat(USER_MESSAGE_TEXT_GAP_COLS),
                                    Style::default().bg(user_bg),
                                ),
                                Span::styled(
                                    badge.to_string(),
                                    Style::default()
                                        .bg(user_bg)
                                        .fg(theme.warn())
                                        .add_modifier(Modifier::ITALIC),
                                ),
                                Span::styled(
                                    padded_tail(user_content_w, used),
                                    Style::default().bg(user_bg),
                                ),
                                Span::styled(
                                    user_gutter.clone(),
                                    Style::default().bg(theme.surface()),
                                ),
                            ];
                            spans.shrink_to_fit();
                            let line = Line::from(spans);
                            let rect = Rect::new(area.x, *current_y, area.width, 1);
                            frame.render_widget(Paragraph::new(line), rect);
                            *current_y += 1;
                        }
                    }
                }

                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
                        break;
                    }

                    let line = if is_user {
                        // Sent user messages render on a dimmer `user_panel_bg`
                        // band. Selection is character-level, not line-level,
                        // so the user can highlight arbitrary substrings.
                        let bg = user_bg;
                        let text_style = Style::default().bg(bg).fg(theme.muted());
                        let sel_style = Style::default().bg(theme.selected()).fg(theme.fg());
                        let sel = line_selection(sel_range, wl);

                        let mut spans = vec![
                            Span::styled(user_gutter.clone(), Style::default().bg(theme.surface())),
                            Span::styled(
                                " ".repeat(USER_MESSAGE_TEXT_GAP_COLS),
                                Style::default().bg(bg),
                            ),
                        ];

                        match sel {
                            None => {
                                spans.push(Span::styled(wl.text.clone(), text_style));
                            }
                            Some((lo, hi)) => {
                                if lo > 0 {
                                    spans.push(Span::styled(wl.text[..lo].to_string(), text_style));
                                }
                                spans.push(Span::styled(wl.text[lo..hi].to_string(), sel_style));
                                if hi < wl.text.len() {
                                    spans.push(Span::styled(wl.text[hi..].to_string(), text_style));
                                }
                            }
                        }

                        let used = USER_MESSAGE_TEXT_GAP_COLS + wl.text.width();
                        spans.push(Span::styled(
                            padded_tail(user_content_w, used),
                            Style::default().bg(bg),
                        ));
                        spans.push(Span::styled(
                            user_gutter.clone(),
                            Style::default().bg(theme.surface()),
                        ));
                        Line::from(spans)
                    } else {
                        let prefix = " ".repeat(TRANSCRIPT_BODY_PREFIX_COLS as usize);
                        line_spans_rich(
                            &prefix,
                            Style::default(),
                            &wl.text,
                            wl.start_byte,
                            line_selection(sel_range, wl),
                            code_ranges,
                            base,
                            theme.code_text(),
                            theme.body(),
                            theme.selected(),
                        )
                    };
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        // User panels prefix text with the outer gutter plus a
                        // one-column gap; other roles use the body prefix.
                        let prefix_cols = if is_user {
                            (USER_MESSAGE_OUTER_GUTTER_COLS + USER_MESSAGE_TEXT_GAP_COLS) as u16
                        } else {
                            TRANSCRIPT_BODY_PREFIX_COLS
                        };
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }

                if is_user {
                    for _ in 0..USER_MESSAGE_TRANSITION_ROWS {
                        *content_lines += 1;
                        if *skip_rows > 0 {
                            *skip_rows = skip_rows.saturating_sub(1);
                        } else if *current_y < area.y + area.height {
                            // Bottom edge: upper-half blocks so only the top half
                            // carries user_panel_bg, meeting the text rows above.
                            let pad = Line::from(vec![
                                Span::styled(
                                    user_gutter.clone(),
                                    Style::default().bg(theme.surface()),
                                ),
                                Span::styled(
                                    "▀".repeat(user_content_w),
                                    Style::default().fg(user_bg).bg(theme.surface()),
                                ),
                                Span::styled(
                                    user_gutter.clone(),
                                    Style::default().bg(theme.surface()),
                                ),
                            ]);
                            let rect = Rect::new(area.x, *current_y, area.width, 1);
                            frame.render_widget(Paragraph::new(pad), rect);
                            *current_y += 1;
                        }
                    }
                }
            }
            Block::Table {
                headers,
                rows,
                aligns,
                ..
            } => {
                // Adaptive table rendering: compute column widths that fit the
                // available terminal width, wrap cell contents within their
                // columns, and draw the grid line-by-line. This keeps borders
                // intact even for wide/CJK tables instead of letting the
                // generic line wrapper mangle `│` separators.
                let indent = 3usize;
                let full_width = area.width as usize;
                // `indent` left + 2-col right gutter (`TRANSCRIPT_H_INSET`).
                let available = full_width.saturating_sub(indent + 2);
                let table = build_table_render(headers, rows, aligns, available);
                let ncols = headers.len().max(1);

                let base = Style::default().fg(theme.fg());
                let border_style = Style::default().fg(theme.muted());
                let sel_bg = theme.selected();

                // A whole-table selection (middle-click) still copies the grid
                // with borders stripped, so keep recording the displayed grid.
                if record_layout {
                    layout_map.record_table_grid(mi, bi, table.lines.join("\n"));
                }

                // If a single cell is selected in this block, resolve its
                // (row, col) so we can highlight just that cell's column across
                // every grid line it spans (including wrapped continuation
                // lines), without bleeding into adjacent cells.
                let selected_cell = match selection {
                    SelectionState::TableCell {
                        message_idx,
                        block_idx,
                        cell_idx,
                    } if *message_idx == mi && *block_idx == bi => {
                        Some((cell_idx / ncols, cell_idx % ncols))
                    }
                    _ => None,
                };

                *content_lines += table.lines.len();
                let mut line_start_byte = 0usize;
                for (line_idx, line_text) in table.lines.iter().enumerate() {
                    let row_info = table.line_info.get(line_idx).and_then(|o| o.as_ref());
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        line_start_byte += line_text.len() + 1; // +1 for '\n'
                        continue;
                    }
                    if *current_y >= area.y + area.height {
                        break;
                    }
                    let is_border = row_info.is_none();

                    let start_byte = line_start_byte;
                    let end_byte = line_start_byte + line_text.len();
                    let wl = WrappedLine {
                        text: line_text.clone(),
                        start_byte,
                        end_byte,
                    };

                    // The byte range to highlight on this line: either the
                    // selected cell's column (cell selection), a whole-line /
                    // partial range (block/range selection), or nothing.
                    let selected_span = if let Some((sr, sc)) = selected_cell {
                        row_info
                            .filter(|info| info.row == sr)
                            .and_then(|info| info.col_spans.get(sc).copied())
                    } else {
                        line_selection(sel_range, &wl)
                    };
                    let fully_selected =
                        matches!(selected_span, Some((s, e)) if s == 0 && e == line_text.len());
                    let pad_style = if fully_selected {
                        Style::default().bg(sel_bg)
                    } else {
                        Style::default()
                    };

                    let used = indent + line_text.width();
                    let mut spans = vec![Span::styled(" ".repeat(indent), pad_style)];
                    // On data lines the `│` rules and inter-cell padding are
                    // border decoration; only the padded cell text (col_spans)
                    // is "content". Paint borders with the same muted style as
                    // the horizontal separators so the grid reads as one
                    // uniform weight — otherwise the vertical rules (drawn on
                    // every data row with the brighter text colour) look
                    // heavier than the sparse horizontal rules.
                    if let Some(info) = row_info {
                        let mut pos = 0usize;
                        for &(lo, hi) in &info.col_spans {
                            if lo > pos {
                                push_table_segment(
                                    &mut spans,
                                    line_text,
                                    pos,
                                    lo,
                                    border_style,
                                    selected_span,
                                    sel_bg,
                                );
                            }
                            push_table_segment(
                                &mut spans,
                                line_text,
                                lo,
                                hi,
                                base,
                                selected_span,
                                sel_bg,
                            );
                            pos = hi;
                        }
                        if pos < line_text.len() {
                            push_table_segment(
                                &mut spans,
                                line_text,
                                pos,
                                line_text.len(),
                                border_style,
                                selected_span,
                                sel_bg,
                            );
                        }
                    } else {
                        push_table_segment(
                            &mut spans,
                            line_text,
                            0,
                            line_text.len(),
                            border_style,
                            selected_span,
                            sel_bg,
                        );
                    }
                    spans.push(Span::styled(padded_tail(full_width, used), pad_style));
                    let line = Line::from(spans);
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        // Register a hit box per cell so clicks resolve to a
                        // single cell (and thus its full, possibly wrapped
                        // text) instead of the whole grid line.
                        if let Some(info) = row_info {
                            for (ci, &(lo, hi)) in info.col_spans.iter().enumerate() {
                                if hi <= lo {
                                    continue;
                                }
                                let col_start = line_text[..lo].width();
                                let col_w = line_text[lo..hi].width();
                                let rect = Rect::new(
                                    area.x + indent as u16 + col_start as u16,
                                    *current_y,
                                    col_w as u16,
                                    1,
                                );
                                layout_map.push_table_cell_hit(TableCellHit {
                                    message_idx: mi,
                                    block_idx: bi,
                                    cell_idx: info.row * ncols + ci,
                                    rect,
                                });
                            }
                        }
                        // Data lines also carry a region so non-table hit
                        // tests (e.g. step headers) keep working; border rules
                        // remain dead zones.
                        if !is_border {
                            layout_map.push(BlockRegion {
                                message_idx: mi,
                                block_idx: bi,
                                start_byte,
                                end_byte,
                                text: line_text.clone(),
                                prefix_cols: indent as u16,
                                rect: line_rect,
                            });
                        }
                    }

                    line_start_byte = end_byte + 1; // +1 for '\n'
                    *current_y += 1;
                }
            }
            Block::Code { language, content } => {
                // Borderless code block: a uniform `code_bg` band with a
                // line-number gutter, matching opencode's clean look. No
                // `╭─ ╰─` frame, no per-line `│` rule.
                let code_bg = theme.body();
                // The solid-background band is inset from the transcript edges so it
                // reads as a distinct panel rather than bleeding into the
                // terminal frame. Content (gutter + code) lives inside the
                // band; the surrounding cells keep `app_bg`.
                let h_inset: u16 = 2;
                let band_x = area.x + h_inset;
                let band_w = area.width.saturating_sub(2 * h_inset).max(1);
                let full_width = band_w as usize;

                // Split into logical lines, tracking each one's byte offset
                // within `content` so semantic selection maps back to the raw
                // source even after per-line wrapping.
                let mut logical_lines: Vec<(usize, &str)> = Vec::new();
                let mut offset = 0usize;
                for line in content.split('\n') {
                    logical_lines.push((offset, line));
                    offset += line.len() + 1; // +1 for the '\n'
                }

                let gutter_width = logical_lines.len().to_string().len().max(2);
                // The code band is a uniform background with a line-number
                // gutter — no left accent bar.
                let left_indent = 2usize;
                let gutter_gap = 1usize;
                let indent = left_indent + 1 /* space */ + gutter_width + gutter_gap;
                let wrap_width = full_width.saturating_sub(indent + 1);

                // Subtle language tag on its own dim line above the gutter.
                if let Some(lang) = language.as_deref().filter(|l| !l.is_empty()) {
                    *content_lines += 1;
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                    } else if *current_y < area.y + area.height {
                        let used = left_indent + 1 + lang.len();
                        let line = Line::from(vec![
                            Span::styled(" ".repeat(left_indent), Style::default().bg(code_bg)),
                            Span::styled(" ", Style::default().bg(code_bg)),
                            Span::styled(
                                lang.to_string(),
                                Style::default().bg(code_bg).fg(theme.dim()),
                            ),
                            Span::styled(
                                padded_tail(full_width, used),
                                Style::default().bg(code_bg),
                            ),
                        ]);
                        let line_rect = Rect::new(band_x, *current_y, band_w, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);
                        *current_y += 1;
                    }
                }

                for (line_idx, (line_start_byte, logical_line)) in logical_lines.iter().enumerate()
                {
                    let wrapped = wrap_text(logical_line, wrap_width);
                    let wrapped: Vec<WrappedLine> = if wrapped.is_empty() {
                        vec![WrappedLine {
                            text: String::new(),
                            start_byte: 0,
                            end_byte: 0,
                        }]
                    } else {
                        wrapped
                    };
                    *content_lines += wrapped.len();
                    for (wrap_idx, wl) in wrapped.iter().enumerate() {
                        if *skip_rows > 0 {
                            *skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if *current_y >= area.y + area.height {
                            break;
                        }

                        let gutter = if wrap_idx == 0 {
                            format!("{:>width$}", line_idx + 1, width = gutter_width)
                        } else {
                            " ".repeat(gutter_width)
                        };

                        // Shift the wrapped line's byte offsets back into
                        // block-content coordinates for selection intersection.
                        let block_wl = WrappedLine {
                            text: wl.text.clone(),
                            start_byte: line_start_byte + wl.start_byte,
                            end_byte: line_start_byte + wl.end_byte,
                        };

                        let line = code_gutter_line(
                            None,
                            left_indent,
                            &gutter,
                            gutter_gap,
                            code_bg,
                            theme.dim(),
                            &wl.text,
                            line_selection(sel_range, &block_wl),
                            theme.code_text(),
                            theme.selected(),
                            full_width,
                        );
                        let line_rect = Rect::new(band_x, *current_y, band_w, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);

                        if record_layout {
                            layout_map.push(BlockRegion {
                                message_idx: mi,
                                block_idx: bi,
                                start_byte: line_start_byte + wl.start_byte,
                                end_byte: line_start_byte + wl.end_byte,
                                text: wl.text.clone(),
                                prefix_cols: indent as u16,
                                rect: line_rect,
                            });
                        }

                        *current_y += 1;
                    }
                }
            }
            Block::Heading {
                level,
                content,
                code_ranges,
            } => {
                let prefix = "   ".to_string();
                let prefix_cols = display_width_u16(&prefix);
                let modifier = if *level == 1 {
                    Modifier::BOLD | Modifier::UNDERLINED
                } else {
                    Modifier::BOLD
                };
                let style = Style::default().fg(theme.heading()).add_modifier(modifier);
                let continuation = " ".repeat(prefix_cols as usize);
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols + 2) as usize);
                *content_lines += lines.len();
                for (line_index, wl) in lines.iter().enumerate() {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
                        break;
                    }
                    let line = line_spans_rich(
                        if line_index == 0 {
                            &prefix
                        } else {
                            &continuation
                        },
                        style,
                        &wl.text,
                        wl.start_byte,
                        line_selection(sel_range, wl),
                        code_ranges,
                        style,
                        theme.code_text(),
                        theme.body(),
                        theme.selected(),
                    );
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }
            }
            Block::Quote {
                content,
                code_ranges,
            } => {
                // 5-col `▎` prefix + 2-col right gutter (`TRANSCRIPT_H_INSET`).
                let lines = wrap_text(content, area.width.saturating_sub(7) as usize);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
                        break;
                    }

                    let base = Style::default().fg(theme.quote());
                    let line = line_spans_rich(
                        "   ▎ ",
                        Style::default().fg(theme.quote()),
                        &wl.text,
                        wl.start_byte,
                        line_selection(sel_range, wl),
                        code_ranges,
                        base,
                        theme.code_text(),
                        theme.body(),
                        theme.selected(),
                    );
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 5,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }
            }
            Block::Rule => {
                *content_lines += 1;
                if *skip_rows > 0 {
                    *skip_rows = skip_rows.saturating_sub(1);
                } else if *current_y < area.y + area.height {
                    let width = area.width.saturating_sub(6) as usize;
                    let text = format!("   {}", "─".repeat(width));
                    let line =
                        Line::from(vec![Span::styled(text, Style::default().fg(theme.dim()))]);
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);
                    *current_y += 1;
                }
            }
            Block::Break => {
                // Visual break, just skip a line
                *content_lines += 1;
                if *skip_rows > 0 {
                    *skip_rows = skip_rows.saturating_sub(1);
                } else if *current_y < area.y + area.height {
                    *current_y += 1;
                }
            }
            Block::ListItem {
                content,
                code_ranges,
                ordered,
                depth,
                checked,
            } => {
                let marker = match (checked, ordered) {
                    (Some(true), _) => "[x]".to_string(),
                    (Some(false), _) => "[ ]".to_string(),
                    (None, Some(index)) => format!("{}.", index),
                    (None, None) => "•".to_string(),
                };
                let indent = "  ".repeat(*depth);
                let prefix = format!("   {}{} ", indent, marker);
                let prefix_cols = display_width_u16(&prefix);
                let continuation = " ".repeat(prefix_cols as usize);
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols + 2) as usize);
                *content_lines += lines.len();
                for (line_index, wl) in lines.iter().enumerate() {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
                        break;
                    }

                    let base = Style::default().fg(theme.fg());
                    let line = line_spans_rich(
                        if line_index == 0 {
                            &prefix
                        } else {
                            &continuation
                        },
                        Style::default().fg(theme.brand()),
                        &wl.text,
                        wl.start_byte,
                        line_selection(sel_range, wl),
                        code_ranges,
                        base,
                        theme.code_text(),
                        theme.body(),
                        theme.selected(),
                    );
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }
            }
        }
    }
}
