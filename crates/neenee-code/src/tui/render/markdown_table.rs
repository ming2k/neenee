//! Adaptive GFM-style table grid layout. Sizes columns to their widest cell,
//! proportionally shrinks them when the table would overflow, wraps cell text
//! to the allotted width, and produces the rendered border + data lines along
//! with per-cell byte spans used by selection and click hit-testing.

use neenee_tui::{
    Span, {Color, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::document::{scan_inline, CodeRange, TableAlignment};

use super::text_layout::{wrap_text, WrappedLine};

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
    /// `(byte_start, byte_end)` of the actual cell text content (without
    /// alignment padding) within the line text. A sub-range of `col_spans`.
    pub col_content_spans: Vec<(usize, usize)>,
    /// Start byte of this line's cell content within the original (unwrapped,
    /// unpadded) cell text. Used to map code/bold ranges onto the displayed
    /// substring.
    pub col_offsets: Vec<usize>,
    /// Per-cell original code ranges (absolute, relative to the original
    /// cell text). Same for every wrapped line of the same cell.
    pub col_code_ranges: Vec<Vec<CodeRange>>,
    /// Per-cell original bold ranges (absolute, relative to the original
    /// cell text). Same for every wrapped line of the same cell.
    pub col_bold_ranges: Vec<Vec<CodeRange>>,
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

    // Per-column intrinsic width. `.take(ncols)` ignores any cells beyond the
    // header width (GFM drops over-wide cells); short rows simply contribute
    // fewer candidates, which is correct for a column-wise maximum.
    let dwidth = |s: &str| s.width();
    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate() {
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

    // Scan inline code / bold ranges for every cell (original text, before
    // wrapping or padding). These ranges use byte offsets relative to the
    // start of each cell's original string. Iterate by column index over
    // `0..ncols` so a ragged row yields empty styles for its missing cells
    // rather than producing a short vec that `format_data_line` would
    // out-of-bounds index (`cell_styles[i]`).
    let header_cell_styles: Vec<(Vec<CodeRange>, Vec<CodeRange>)> =
        headers.iter().map(|h| scan_inline(h)).collect();
    let row_cell_styles: Vec<Vec<(Vec<CodeRange>, Vec<CodeRange>)>> = rows
        .iter()
        .map(|row| {
            (0..ncols)
                .map(|i| {
                    row.get(i)
                        .map(|cell| scan_inline(cell))
                        .unwrap_or_else(|| (Vec::new(), Vec::new()))
                })
                .collect()
        })
        .collect();

    // Wrap each cell to its (possibly shrunk) column width, preserving byte
    // offsets via WrappedLine.
    let wrap_cell = |cell: &str, w: usize| -> Vec<WrappedLine> {
        if cell.is_empty() {
            return vec![WrappedLine {
                text: String::new(),
                start_byte: 0,
                end_byte: 0,
            }];
        }
        let wrapped = wrap_text(cell, w.max(1));
        if wrapped.is_empty() {
            vec![WrappedLine {
                text: String::new(),
                start_byte: 0,
                end_byte: 0,
            }]
        } else {
            wrapped
        }
    };

    let wrapped_headers: Vec<Vec<WrappedLine>> = (0..ncols)
        .map(|i| wrap_cell(&headers[i], widths[i]))
        .collect();
    let wrapped_rows: Vec<Vec<Vec<WrappedLine>>> = rows
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

    // Build one data line from a row of wrapped cells, returning the line
    // text together with per-cell geometry and style metadata.
    let format_data_line =
        |cells: &[Vec<WrappedLine>],
         cell_styles: &[(Vec<CodeRange>, Vec<CodeRange>)],
         line_idx: usize|
         -> (String, TableRowInfo) {
            let mut line = String::from("│ ");
            let mut col_spans = Vec::with_capacity(ncols);
            let mut col_content_spans = Vec::with_capacity(ncols);
            let mut col_offsets = Vec::with_capacity(ncols);
            let mut col_code_ranges = Vec::with_capacity(ncols);
            let mut col_bold_ranges = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let wl = cells[i].get(line_idx);
                let cell_line = wl.map(|w| w.text.as_str()).unwrap_or("");
                let cell_start_byte = wl.map(|w| w.start_byte).unwrap_or(0);
                let align = aligns.get(i).copied().unwrap_or(TableAlignment::None);

                let part = pad_cell_text(cell_line, widths[i], align);

                // Compute where the actual cell content (without padding)
                // sits within the padded string.
                let cell_w = cell_line.width();
                let pad = widths[i].saturating_sub(cell_w);
                let (content_lo_in_part, content_hi_in_part) = match align {
                    TableAlignment::Right => (pad, pad + cell_w),
                    TableAlignment::Center => {
                        let left = pad / 2;
                        (left, left + cell_w)
                    }
                    TableAlignment::None | TableAlignment::Left => (0, cell_w),
                };

                let padded_start = line.len();
                line.push_str(&part);
                let padded_end = line.len();

                col_spans.push((padded_start, padded_end));
                col_content_spans.push((
                    padded_start + content_lo_in_part,
                    padded_start + content_hi_in_part,
                ));
                col_offsets.push(cell_start_byte);

                let (cr, br) = &cell_styles[i];
                col_code_ranges.push(cr.clone());
                col_bold_ranges.push(br.clone());

                if i + 1 < ncols {
                    line.push_str(" │ ");
                }
            }
            line.push_str(" │");
            (
                line,
                TableRowInfo {
                    row: 0, // filled by caller
                    col_spans,
                    col_content_spans,
                    col_offsets,
                    col_code_ranges,
                    col_bold_ranges,
                },
            )
        };

    let mut lines = Vec::new();
    let mut line_info: Vec<Option<TableRowInfo>> = Vec::new();

    lines.push(format!("┌{}┐", join_horizontal("┬")));
    line_info.push(None);

    let header_height = wrapped_headers.iter().map(|v| v.len()).max().unwrap_or(1);
    for line_idx in 0..header_height {
        let (l, mut info) = format_data_line(&wrapped_headers, &header_cell_styles, line_idx);
        info.row = 0;
        lines.push(l);
        line_info.push(Some(info));
    }

    lines.push(format!("├{}┤", join_horizontal("┼")));
    line_info.push(None);

    let separator = format!("├{}┤", join_horizontal("┼"));

    for (row_idx, wrapped_row) in wrapped_rows.iter().enumerate() {
        let row_height = wrapped_row.iter().map(|v| v.len()).max().unwrap_or(1);
        let cell_styles = &row_cell_styles[row_idx];
        for line_idx in 0..row_height {
            let (l, mut info) = format_data_line(wrapped_row, cell_styles, line_idx);
            info.row = row_idx + 1;
            lines.push(l);
            line_info.push(Some(info));
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
