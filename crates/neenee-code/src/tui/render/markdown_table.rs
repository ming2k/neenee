//! Adaptive GFM-style table grid layout. Sizes columns to their widest cell,
//! proportionally shrinks them when the table would overflow, wraps cell text
//! to the allotted width, and produces the rendered border + data lines along
//! with per-cell byte spans used by selection and click hit-testing.

use ratatui::{
    style::{Color, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::document::TableAlignment;

use super::text_layout::wrap_text;

/// Push one segment of a table grid line, splitting it around the selection
/// overlay (if any). `seg_lo`/`seg_hi` are byte offsets within `text`;
/// `style` is the base style for the segment (border vs. content); `sel_bg`
/// is painted under the selected portion.
pub(super) fn push_table_segment(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    seg_lo: usize,
    seg_hi: usize,
    style: Style,
    sel: Option<(usize, usize)>,
    sel_bg: Color,
) {
    if seg_hi <= seg_lo {
        return;
    }
    match sel {
        Some((slo, shi)) if slo < seg_hi && seg_lo < shi => {
            let cut_lo = slo.max(seg_lo) - seg_lo;
            let cut_hi = shi.min(seg_hi) - seg_lo;
            let segment = &text[seg_lo..seg_hi];
            if cut_lo > 0 {
                spans.push(Span::styled(segment[..cut_lo].to_string(), style));
            }
            spans.push(Span::styled(
                segment[cut_lo..cut_hi].to_string(),
                style.bg(sel_bg),
            ));
            if cut_hi < segment.len() {
                spans.push(Span::styled(segment[cut_hi..].to_string(), style));
            }
        }
        _ => {
            spans.push(Span::styled(text[seg_lo..seg_hi].to_string(), style));
        }
    }
}

/// Result of laying out a table: the rendered grid lines plus, for each data
/// line, the row index and the byte span of each column's (padded) content
/// within that line. Border lines carry `None`. The spans let the renderer
/// highlight one cell at a time and resolve clicks to a specific cell.
pub(super) struct TableRender {
    pub lines: Vec<String>,
    pub line_info: Vec<Option<TableRowInfo>>,
}

pub(super) struct TableRowInfo {
    pub row: usize,
    /// `(byte_start, byte_end)` of each column's padded content within the
    /// line text. Length equals the column count.
    pub col_spans: Vec<(usize, usize)>,
}

/// Build the visual lines of a GFM-style table grid that fits within
/// `max_width` display columns. Columns are sized to their widest cell
/// (intrinsic width) when space allows; when the table would overflow,
/// columns shrink proportionally to a minimum of 3 columns and cell text
/// wraps within the allotted width.
pub(super) fn build_table_render(
    headers: &[String],
    rows: &[Vec<String>],
    aligns: &[TableAlignment],
    max_width: usize,
) -> TableRender {
    let ncols = headers.len();
    if ncols == 0 {
        return TableRender {
            lines: Vec::new(),
            line_info: Vec::new(),
        };
    }

    // Per-column intrinsic width.
    let dwidth = |s: &str| s.width();
    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate().take(ncols) {
        widths[i] = widths[i].max(dwidth(h));
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(dwidth(cell));
        }
    }

    // "│ cell │ cell │": each column contributes width + 2 padding, plus 1
    // separator per column boundary. Total = sum(widths) + 3*ncols + 1.
    let border_overhead = 3 * ncols + 1;
    let total: usize = widths.iter().sum::<usize>() + border_overhead;
    if total > max_width {
        let content_available = max_width.saturating_sub(border_overhead);
        widths = shrink_column_widths(&widths, content_available, 3);
    }

    // Wrap each cell to its (possibly shrunk) column width.
    let wrap_cell = |cell: &str, w: usize| -> Vec<String> {
        if cell.is_empty() {
            return vec![String::new()];
        }
        let wrapped = wrap_text(cell, w.max(1));
        if wrapped.is_empty() {
            vec![String::new()]
        } else {
            wrapped.into_iter().map(|wl| wl.text).collect()
        }
    };

    let wrapped_headers: Vec<Vec<String>> = (0..ncols)
        .map(|i| wrap_cell(&headers[i], widths[i]))
        .collect();
    let wrapped_rows: Vec<Vec<Vec<String>>> = rows
        .iter()
        .map(|row| {
            (0..ncols)
                .map(|i| wrap_cell(row.get(i).map(String::as_str).unwrap_or(""), widths[i]))
                .collect()
        })
        .collect();

    let join_horizontal = |sep: &str| -> String {
        widths
            .iter()
            .map(|w| "─".repeat(w + 2))
            .collect::<Vec<_>>()
            .join(sep)
    };

    // Build one data line and record each column's padded-content byte span.
    let format_data_line =
        |cells: &[Vec<String>], line_idx: usize| -> (String, Vec<(usize, usize)>) {
            let mut line = String::from("│ ");
            let mut spans = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let cell_line = cells[i].get(line_idx).map(String::as_str).unwrap_or("");
                let part = pad_cell_text(
                    cell_line,
                    widths[i],
                    aligns.get(i).copied().unwrap_or(TableAlignment::None),
                );
                let start = line.len();
                line.push_str(&part);
                spans.push((start, line.len()));
                if i + 1 < ncols {
                    line.push_str(" │ ");
                }
            }
            line.push_str(" │");
            (line, spans)
        };

    let mut lines = Vec::new();
    let mut line_info: Vec<Option<TableRowInfo>> = Vec::new();

    lines.push(format!("┌{}┐", join_horizontal("┬")));
    line_info.push(None);

    let header_height = wrapped_headers.iter().map(|v| v.len()).max().unwrap_or(1);
    for line_idx in 0..header_height {
        let (l, spans) = format_data_line(&wrapped_headers, line_idx);
        lines.push(l);
        line_info.push(Some(TableRowInfo {
            row: 0,
            col_spans: spans,
        }));
    }

    lines.push(format!("├{}┤", join_horizontal("┼")));
    line_info.push(None);

    let separator = format!("├{}┤", join_horizontal("┼"));

    for (row_idx, wrapped_row) in wrapped_rows.iter().enumerate() {
        let row_height = wrapped_row.iter().map(|v| v.len()).max().unwrap_or(1);
        for line_idx in 0..row_height {
            let (l, spans) = format_data_line(wrapped_row, line_idx);
            lines.push(l);
            line_info.push(Some(TableRowInfo {
                row: row_idx + 1,
                col_spans: spans,
            }));
        }
        // Horizontal separator between body rows (not after the last one).
        if row_idx + 1 < wrapped_rows.len() {
            lines.push(separator.clone());
            line_info.push(None);
        }
    }

    lines.push(format!("└{}┘", join_horizontal("┴")));
    line_info.push(None);

    TableRender { lines, line_info }
}

/// Proportionally shrink column widths so they fit within `target` display
/// columns. Each column keeps at least `min_col` characters; the remaining
/// budget is distributed in proportion to how much above the minimum each
/// column's intrinsic width is.
pub(super) fn shrink_column_widths(
    intrinsic: &[usize],
    target: usize,
    min_col: usize,
) -> Vec<usize> {
    let ncols = intrinsic.len();
    if ncols == 0 {
        return Vec::new();
    }
    let total_min = min_col * ncols;
    if target <= total_min {
        return vec![min_col; ncols];
    }
    let total_intrinsic: usize = intrinsic.iter().sum();
    let shrinkable = total_intrinsic.saturating_sub(total_min);
    if shrinkable == 0 {
        return intrinsic.to_vec();
    }
    let available = target - total_min;
    intrinsic
        .iter()
        .map(|&w| {
            let above_min = w.saturating_sub(min_col);
            min_col + above_min * available / shrinkable
        })
        .collect()
}

pub(super) fn pad_cell_text(cell: &str, width: usize, align: TableAlignment) -> String {
    let cell_w = cell.width();
    let pad = width.saturating_sub(cell_w);
    match align {
        TableAlignment::Right => format!("{}{}", " ".repeat(pad), cell),
        TableAlignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right))
        }
        TableAlignment::None | TableAlignment::Left => format!("{}{}", cell, " ".repeat(pad)),
    }
}
