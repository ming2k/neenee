//! Markdown block-level rendering for a single message: text, code, tables,
//! headings, quotes, lists, rules, breaks. Emits one rendered line per row
//! and records semantic [`BlockRegion`]s / table cell hit boxes for selection
//! and click hit-testing.

use neenee_tui::{Color, Frame, Line, Modifier, Paragraph, Rect, Span, Style};
use unicode_width::UnicodeWidthStr;

use crate::tui::document::{Block, DeliveryStatus, TranscriptMessage};
use crate::tui::layout::{BlockRegion, LayoutMap, TableCellHit, TableCellSegment};
use crate::tui::selection::{
    CellDragInfo, SelectionState, floor_grapheme_boundary, inclusive_grapheme_end,
};

use super::design::{
    USER_MESSAGE_OUTER_GUTTER_COLS, USER_MESSAGE_RIGHT_PAD_COLS, USER_MESSAGE_TEXT_GAP_COLS,
    USER_MESSAGE_TRANSITION_ROWS,
};
use super::markdown_table::{TableRowInfo, build_table_render, push_table_segment};
use super::text_layout::{
    WrappedLine, block_selection_range, bold_delim_local_ranges, code_gutter_line, line_selection,
    line_spans_rich, markup_hidden_ranges, padded_tail, visible_width, wrap_text,
};
use super::{TRANSCRIPT_BODY_LEADING_INDENT, Theme};

fn display_width_u16(s: &str) -> u16 {
    s.width() as u16
}

fn table_line_hidden_ranges(line_text: &str, info: &TableRowInfo) -> Vec<(usize, usize)> {
    let mut hidden = Vec::new();
    for ci in 0..info.col_content_spans.len() {
        let (clo, chi) = info.col_content_spans[ci];
        if chi <= clo {
            continue;
        }
        let offset = info.col_offsets.get(ci).copied().unwrap_or(0);
        let code_ranges = info
            .col_code_ranges
            .get(ci)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let bold_ranges = info
            .col_bold_ranges
            .get(ci)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        hidden.extend(
            markup_hidden_ranges(&line_text[clo..chi], offset, code_ranges, bold_ranges)
                .into_iter()
                .map(|(lo, hi)| (clo + lo, clo + hi)),
        );
    }
    hidden
}

fn visible_width_window(
    text: &str,
    start: usize,
    end: usize,
    hidden_ranges: &[(usize, usize)],
) -> usize {
    let local_hidden: Vec<(usize, usize)> = hidden_ranges
        .iter()
        .filter_map(|&(lo, hi)| {
            let clipped_lo = lo.max(start);
            let clipped_hi = hi.min(end);
            (clipped_lo < clipped_hi).then(|| (clipped_lo - start, clipped_hi - start))
        })
        .collect();
    visible_width(&text[start..end], &local_hidden)
}

fn cell_drag_selected_span(
    selection: &SelectionState,
    cell: &CellDragInfo,
    line_start_byte: usize,
    line_text: &str,
) -> Option<(usize, usize)> {
    let (start, end) = selection.active_normalized_range()?;
    let sel_start = start.byte_offset;
    let sel_end = end.byte_offset;
    let line_end_byte = line_start_byte + line_text.len();
    let mut out: Option<(usize, usize)> = None;

    for segment in &cell.segments {
        let segment_start = segment.content_range.0.max(line_start_byte);
        let segment_end = segment.content_range.1.min(line_end_byte);
        if segment_start >= segment_end || sel_end < segment_start || sel_start > segment_end {
            continue;
        }

        let raw_lo_abs = sel_start.max(segment_start);
        let raw_hi_abs = if sel_end < segment.content_range.1 {
            sel_end.min(segment_end)
        } else {
            segment_end
        };
        if raw_lo_abs > raw_hi_abs {
            continue;
        }

        let lo = floor_grapheme_boundary(line_text, raw_lo_abs - line_start_byte);
        let hi = if sel_end < segment.content_range.1 {
            inclusive_grapheme_end(line_text, raw_hi_abs - line_start_byte)
        } else {
            raw_hi_abs - line_start_byte
        };
        if lo < hi {
            out = Some(match out {
                Some((old_lo, old_hi)) => (old_lo.min(lo), old_hi.max(hi)),
                None => (lo, hi),
            });
        }
    }

    out
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
    cell_selection: Option<&CellDragInfo>,
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
                bold_ranges,
            } => {
                let is_user = msg.role == neenee_core::Role::User;
                let is_queued = is_user && msg.delivery == DeliveryStatus::Queued;
                let base = match msg.role {
                    neenee_core::Role::User => Style::default().fg(theme.user_text()),
                    neenee_core::Role::System => Style::default().fg(theme.system_text()),
                    _ => Style::default().fg(theme.fg()),
                };
                let full_width = area.width as usize;
                // The horizontal gutter is applied once at the stream entry
                // point, so only the leading indent remains to subtract here.
                let body_wrap_width =
                    area.width.saturating_sub(TRANSCRIPT_BODY_LEADING_INDENT) as usize;
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
                        let prefix = " ".repeat(TRANSCRIPT_BODY_LEADING_INDENT as usize);
                        line_spans_rich(
                            &prefix,
                            Style::default(),
                            &wl.text,
                            wl.start_byte,
                            line_selection(sel_range, wl),
                            code_ranges,
                            bold_ranges,
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
                            TRANSCRIPT_BODY_LEADING_INDENT
                        };
                        let hidden_ranges = if is_user {
                            Vec::new()
                        } else {
                            bold_delim_local_ranges(&wl.text, wl.start_byte, bold_ranges)
                        };
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                            hidden_ranges,
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
                // The area is already inset; `indent` is the table's left visual indent.
                let available = full_width.saturating_sub(indent);
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
                let cell_drag_for_block = cell_selection.filter(|cell| {
                    cell.message_idx == mi
                        && cell.block_idx == bi
                        && selection.active_normalized_range().is_some()
                });

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
                    let selected_span = if let Some(cell) = cell_drag_for_block {
                        cell_drag_selected_span(selection, cell, start_byte, line_text)
                    } else if let Some((sr, sc)) = selected_cell {
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

                    let hidden_for_line = row_info
                        .map(|info| table_line_hidden_ranges(line_text, info))
                        .unwrap_or_default();
                    let used = indent + visible_width(line_text, &hidden_for_line);
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
                        for i in 0..ncols.min(info.col_spans.len()) {
                            let (lo, hi) = info.col_spans[i];
                            let (clo, chi) =
                                info.col_content_spans.get(i).copied().unwrap_or((lo, hi));
                            let offset = info.col_offsets.get(i).copied().unwrap_or(0);
                            let code_ranges = info
                                .col_code_ranges
                                .get(i)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]);
                            let bold_ranges = info
                                .col_bold_ranges
                                .get(i)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]);

                            // Border / inter-cell separator before this cell
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

                            // Leading alignment padding
                            if clo > lo {
                                push_table_segment(
                                    &mut spans,
                                    line_text,
                                    lo,
                                    clo,
                                    base,
                                    selected_span,
                                    sel_bg,
                                );
                            }

                            // Cell content with inline code / bold styles
                            if chi > clo {
                                let cell_sel = selected_span.and_then(|(slo, shi)| {
                                    if slo < chi && clo < shi {
                                        let cs = slo.max(clo).saturating_sub(clo);
                                        let ce = shi.min(chi).saturating_sub(clo);
                                        if cs < ce { Some((cs, ce)) } else { None }
                                    } else {
                                        None
                                    }
                                });

                                let content_line = line_spans_rich(
                                    "",
                                    Style::default(),
                                    &line_text[clo..chi],
                                    offset,
                                    cell_sel,
                                    code_ranges,
                                    bold_ranges,
                                    base,
                                    theme.code_text(),
                                    theme.body(),
                                    sel_bg,
                                );
                                // Skip the empty-prefix span (position 0).
                                for span in content_line.spans.into_iter().skip(1) {
                                    spans.push(span);
                                }
                            }

                            // Trailing alignment padding
                            if hi > chi {
                                push_table_segment(
                                    &mut spans,
                                    line_text,
                                    chi,
                                    hi,
                                    base,
                                    selected_span,
                                    sel_bg,
                                );
                            }

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
                                let (clo, chi) =
                                    info.col_content_spans.get(ci).copied().unwrap_or((lo, hi));
                                let source_start = info.col_offsets.get(ci).copied().unwrap_or(0);
                                let source_end = source_start + chi.saturating_sub(clo);
                                let col_start =
                                    visible_width_window(line_text, 0, lo, &hidden_for_line);
                                let col_w =
                                    visible_width_window(line_text, lo, hi, &hidden_for_line);
                                let rect = Rect::new(
                                    area.x + indent as u16 + col_start as u16,
                                    *current_y,
                                    col_w as u16,
                                    1,
                                );
                                let cell_text = if info.row == 0 {
                                    headers.get(ci).cloned().unwrap_or_default()
                                } else {
                                    rows.get(info.row.saturating_sub(1))
                                        .and_then(|r| r.get(ci))
                                        .cloned()
                                        .unwrap_or_default()
                                };
                                layout_map.push_table_cell_hit(TableCellHit {
                                    message_idx: mi,
                                    block_idx: bi,
                                    cell_idx: info.row * ncols + ci,
                                    rect,
                                    cell_text,
                                    segment: TableCellSegment {
                                        rendered_range: (start_byte + lo, start_byte + hi),
                                        content_range: (start_byte + clo, start_byte + chi),
                                        source_range: (source_start, source_end),
                                    },
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
                                hidden_ranges: hidden_for_line.clone(),
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
                            Color::Reset,
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
                                hidden_ranges: Vec::new(),
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
                bold_ranges,
            } => {
                let prefix = " ".repeat(TRANSCRIPT_BODY_LEADING_INDENT as usize);
                let prefix_cols = TRANSCRIPT_BODY_LEADING_INDENT;
                let modifier = if *level == 1 {
                    Modifier::BOLD | Modifier::UNDERLINED
                } else {
                    Modifier::BOLD
                };
                let style = Style::default().fg(theme.heading()).add_modifier(modifier);
                // The heading *prefix* (leading `   ` indent and continuation
                // indentation) is decoration, not heading text, so it must not
                // carry the UNDERLINED modifier. Splitting the prefix off the
                // UNDERLINED run is what keeps the underline confined to the
                // actual heading text instead of bleeding left into the indent
                // whitespace (and, for wrapped headings, underlining the whole
                // continuation row's leading blanks).
                let prefix_style = Style::default()
                    .fg(theme.heading())
                    .add_modifier(Modifier::BOLD);
                let continuation = " ".repeat(prefix_cols as usize);
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols) as usize);
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
                        prefix_style,
                        &wl.text,
                        wl.start_byte,
                        line_selection(sel_range, wl),
                        code_ranges,
                        bold_ranges,
                        style,
                        theme.code_text(),
                        theme.body(),
                        theme.selected(),
                    );
                    // For H1 headings the terminal UNDERLINED modifier fills
                    // the entire Paragraph rect, so clamp the render width to
                    // the actual text extent to prevent the underline from
                    // bleeding into trailing whitespace.
                    let full_rect = Rect::new(area.x, *current_y, area.width, 1);
                    let hidden_for_line =
                        bold_delim_local_ranges(&wl.text, wl.start_byte, bold_ranges);
                    let text_cols = prefix_cols + visible_width(&wl.text, &hidden_for_line) as u16;
                    let render_rect = if *level == 1 {
                        Rect::new(area.x, *current_y, text_cols.min(area.width), 1)
                    } else {
                        full_rect
                    };
                    frame.render_widget(Paragraph::new(line), render_rect);

                    if record_layout {
                        // Layout map always uses full width for hit-testing
                        // and selection across the entire line.
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: full_rect,
                            hidden_ranges: hidden_for_line,
                        });
                    }

                    *current_y += 1;
                }
            }
            Block::Quote {
                content,
                code_ranges,
                bold_ranges,
            } => {
                // 5-col `▎` prefix; the area is already inset so no right gutter.
                let lines = wrap_text(content, area.width.saturating_sub(5) as usize);
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
                        bold_ranges,
                        base,
                        theme.code_text(),
                        theme.body(),
                        theme.selected(),
                    );
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        let hidden_for_line =
                            bold_delim_local_ranges(&wl.text, wl.start_byte, bold_ranges);
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 5,
                            rect: line_rect,
                            hidden_ranges: hidden_for_line,
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
                    let width = area.width.saturating_sub(3) as usize;
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
                bold_ranges,
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
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols) as usize);
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
                        bold_ranges,
                        base,
                        theme.code_text(),
                        theme.body(),
                        theme.selected(),
                    );
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        let hidden_for_line =
                            bold_delim_local_ranges(&wl.text, wl.start_byte, bold_ranges);
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                            hidden_ranges: hidden_for_line,
                        });
                    }

                    *current_y += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::document::TableAlignment;

    #[test]
    fn table_markup_hidden_ranges_drive_hitbox_widths() {
        let table = build_table_render(
            &["a".to_string(), "b".to_string()],
            &[vec!["`中`".to_string(), "**ab**".to_string()]],
            &[TableAlignment::None, TableAlignment::None],
            80,
        );
        let row_idx = table
            .lines
            .iter()
            .position(|line| line.contains("`中`"))
            .expect("data row should render");
        let line = &table.lines[row_idx];
        let info = table.line_info[row_idx]
            .as_ref()
            .expect("data row should carry row info");
        let hidden = table_line_hidden_ranges(line, info);

        assert_eq!(visible_width_window(line, 0, line.len(), &hidden), 11);
        for &(lo, hi) in &info.col_spans {
            assert_eq!(visible_width_window(line, lo, hi, &hidden), 2);
            assert!(line[lo..hi].width() > 2, "raw markup width must be larger");
        }
    }

    #[test]
    fn cell_drag_selection_spans_only_origin_cell_segments() {
        use crate::tui::layout::{SemanticCursor, TableCellSegment};

        let selection = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 11),
            head: SemanticCursor::new(0, 0, 100),
        };
        let cell = CellDragInfo {
            message_idx: 0,
            block_idx: 0,
            cell_text: "abcdef".to_string(),
            segments: vec![
                TableCellSegment {
                    rendered_range: (10, 13),
                    content_range: (10, 13),
                    source_range: (0, 3),
                },
                TableCellSegment {
                    rendered_range: (40, 43),
                    content_range: (40, 43),
                    source_range: (3, 6),
                },
            ],
        };

        assert_eq!(
            cell_drag_selected_span(&selection, &cell, 0, &" ".repeat(20)),
            Some((11, 13))
        );
        assert_eq!(
            cell_drag_selected_span(&selection, &cell, 30, &" ".repeat(20)),
            Some((10, 13))
        );
        assert_eq!(
            cell_drag_selected_span(&selection, &cell, 60, &" ".repeat(20)),
            None,
            "rows/cells outside the origin cell must not inherit generic range selection"
        );
    }
}
