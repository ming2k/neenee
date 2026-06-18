//! Rendering engine: draws the chat UI using ratatui while recording
//! semantic-to-screen layout information.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use std::collections::HashMap;

use crate::document::{Block, ChatMessage};
use crate::layout::{BlockRegion, LayoutMap, TableCellHit};
use crate::selection::{floor_char_boundary, inclusive_end, SelectionState};
use neenee_core::{AgentMode, Goal, GoalStatus, PermissionRequest};

/// Global viewport margin. Only vertical breathing room (1 cell top and
/// bottom) is reserved; horizontally every component spans the full terminal
/// width. Per-component insets (e.g. the `┃` accent bar on the input box and
/// user messages) are owned by the components themselves, not by this margin.
const VIEWPORT_H_MARGIN: u16 = 0;
const VIEWPORT_V_MARGIN: u16 = 1;

/// Uniform horizontal inset applied to every chat-area component so no band,
/// bar, or text touches the terminal frame. Both gutters show `app_bg` via the
/// global frame fill. Cards consume this via [`chat_band_rect`]; user panels
/// and code blocks render their own equivalent gutters; markdown text wraps
/// with `CHAT_H_INSET` cells of slack on the right.
const CHAT_H_INSET: u16 = 2;
const PERMISSION_SHEET_MAX_WIDTH: u16 = 118;
const PERMISSION_SHEET_MIN_WIDTH: u16 = 64;
const PERMISSION_SHEET_WIDTH_PERCENT: u16 = 72;
const PERMISSION_SHEET_H_PADDING: u16 = 3;
const PERMISSION_SHEET_TOP_PADDING: u16 = 1;
const PERMISSION_SHEET_BOTTOM_PADDING: u16 = 1;
const PERMISSION_SHEET_INPUT_GAP: u16 = 1;
const PERMISSION_SHEET_MAX_HEIGHT: u16 = 18;

/// The usable area after reserving the global viewport margins (1 cell top
/// and bottom). The full `frame.size()` is only used to paint the app
/// background and the modal backdrop.
fn viewport_rect(frame: &Frame) -> Rect {
    frame.size().inner(&ratatui::layout::Margin {
        horizontal: VIEWPORT_H_MARGIN,
        vertical: VIEWPORT_V_MARGIN,
    })
}

/// Inner rect of a chat-area region after reserving the uniform
/// [`CHAT_H_INSET`] left+right `app_bg` gutters. Use this as the render target
/// for any solid-background band (card headers/bodies, child tool steps) so
/// the band sits inside the gutters rather than spanning edge to edge. The
/// surrounding cells keep `app_bg` from the global frame fill.
fn chat_band_rect(area: Rect) -> Rect {
    Rect::new(
        area.x + CHAT_H_INSET,
        area.y,
        area.width.saturating_sub(2 * CHAT_H_INSET).max(1),
        area.height,
    )
}

/// The byte range of a block covered by the selection.
/// `(start, None)` means "from start to the end of the block".
fn block_selection_range(
    selection: &SelectionState,
    message_idx: usize,
    block_idx: usize,
) -> Option<(usize, Option<usize>)> {
    match selection {
        SelectionState::None => None,
        SelectionState::TableCell { .. } => None,
        SelectionState::Block {
            message_idx: mi,
            block_idx: bi,
        } => (*mi == message_idx && *bi == block_idx).then_some((0, None)),
        SelectionState::Range { .. } => {
            let (start, end) = selection.normalized_range()?;
            let here = (message_idx, block_idx);
            if here < (start.message_idx, start.block_idx)
                || here > (end.message_idx, end.block_idx)
            {
                return None;
            }
            let s = if here == (start.message_idx, start.block_idx) {
                start.byte_offset
            } else {
                0
            };
            let e = if here == (end.message_idx, end.block_idx) {
                Some(end.byte_offset)
            } else {
                None
            };
            Some((s, e))
        }
    }
}

/// Intersect a block selection range with one wrapped line, producing the
/// selected byte range *relative to the line text*. The selection head
/// character is included.
fn line_selection(
    range: Option<(usize, Option<usize>)>,
    wl: &WrappedLine,
) -> Option<(usize, usize)> {
    let (s, e) = range?;
    if let Some(e) = e {
        if e < wl.start_byte {
            return None;
        }
    }
    if s >= wl.end_byte && !(s == wl.start_byte && wl.text.is_empty()) {
        return None;
    }
    let lo = floor_char_boundary(&wl.text, s.saturating_sub(wl.start_byte));
    let hi = match e {
        Some(e) if e < wl.end_byte => inclusive_end(&wl.text, e - wl.start_byte),
        _ => wl.text.len(),
    };
    (lo < hi).then_some((lo, hi))
}

/// Build a rendered line: decoration prefix plus the text split into
/// unselected / selected / unselected spans.
fn line_spans(
    prefix: &str,
    prefix_style: Style,
    text: &str,
    selected: Option<(usize, usize)>,
    base: Style,
    selected_bg: Color,
) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_string(), prefix_style)];
    match selected {
        None => spans.push(Span::styled(text.to_string(), base)),
        Some((lo, hi)) => {
            if lo > 0 {
                spans.push(Span::styled(text[..lo].to_string(), base));
            }
            spans.push(Span::styled(text[lo..hi].to_string(), base.bg(selected_bg)));
            if hi < text.len() {
                spans.push(Span::styled(text[hi..].to_string(), base));
            }
        }
    }
    Line::from(spans)
}

/// Push one segment of a table grid line, splitting it around the selection
/// overlay (if any). `seg_lo`/`seg_hi` are byte offsets within `text`;
/// `style` is the base style for the segment (border vs. content); `sel_bg`
/// is painted under the selected portion.
fn push_table_segment(
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

/// Build a rendered code line with a line-number gutter, a uniform `code_bg`
/// band that fills the full width, and character-level selection highlighting.
/// The optional left bar is retained for callers that want it; code blocks
/// themselves render borderless with only the gutter as ornament.
#[allow(clippy::too_many_arguments)]
fn code_gutter_line(
    left_bar: Option<Color>,
    left_indent: usize,
    gutter: &str,
    gutter_gap: usize,
    code_bg: Color,
    gutter_fg: Color,
    text: &str,
    selected: Option<(usize, usize)>,
    code_fg: Color,
    selected_bg: Color,
    full_width: usize,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut prefix = left_indent;

    spans.push(Span::styled(
        " ".repeat(left_indent),
        Style::default().bg(code_bg),
    ));

    if let Some(bar_color) = left_bar {
        spans.push(Span::styled(
            "┃",
            Style::default().bg(code_bg).fg(bar_color),
        ));
        prefix += 1;
    }

    spans.push(Span::styled(" ", Style::default().bg(code_bg)));
    prefix += 1;

    spans.push(Span::styled(
        gutter.to_string(),
        Style::default().bg(code_bg).fg(gutter_fg),
    ));
    spans.push(Span::styled(
        " ".repeat(gutter_gap),
        Style::default().bg(code_bg),
    ));

    let indent = prefix + gutter.len() + gutter_gap;
    match selected {
        None => {
            spans.push(Span::styled(
                text.to_string(),
                Style::default().fg(code_fg).bg(code_bg),
            ));
        }
        Some((lo, hi)) => {
            if lo > 0 {
                spans.push(Span::styled(
                    text[..lo].to_string(),
                    Style::default().fg(code_fg).bg(code_bg),
                ));
            }
            spans.push(Span::styled(
                text[lo..hi].to_string(),
                Style::default().fg(code_fg).bg(selected_bg),
            ));
            if hi < text.len() {
                spans.push(Span::styled(
                    text[hi..].to_string(),
                    Style::default().fg(code_fg).bg(code_bg),
                ));
            }
        }
    }
    let used = indent + text.width();
    spans.push(Span::styled(
        padded_tail(full_width, used),
        Style::default().bg(code_bg),
    ));
    Line::from(spans)
}

/// A wrapped line with byte-offset bookkeeping.
struct WrappedLine {
    text: String,
    start_byte: usize,
    end_byte: usize,
}

/// Wrap text into lines that fit within `max_width` display columns.
/// Returns each line along with the byte range it covers in the original text.
fn wrap_text(text: &str, max_width: usize) -> Vec<WrappedLine> {
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;
    let mut line_start_byte = 0;

    for (byte_idx, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);

        if ch == '\n' {
            lines.push(WrappedLine {
                text: std::mem::take(&mut current_line),
                start_byte: line_start_byte,
                end_byte: byte_idx,
            });
            line_start_byte = byte_idx + 1;
            current_width = 0;
            continue;
        }

        // Keep closing CJK punctuation with the preceding character. If it
        // would start the next line, move the preceding character with it.
        if current_width + ch_width > max_width && !current_line.is_empty() {
            let move_previous = prohibited_line_start(ch)
                || current_line.chars().last().is_some_and(prohibited_line_end);
            if move_previous && current_line.chars().count() > 1 {
                let moved = current_line.pop().unwrap();
                let moved_start = byte_idx - moved.len_utf8();
                lines.push(WrappedLine {
                    text: std::mem::take(&mut current_line),
                    start_byte: line_start_byte,
                    end_byte: moved_start,
                });
                current_line.push(moved);
                current_width = moved.width().unwrap_or(0);
                line_start_byte = moved_start;
            } else {
                lines.push(WrappedLine {
                    text: std::mem::take(&mut current_line),
                    start_byte: line_start_byte,
                    end_byte: byte_idx,
                });
                line_start_byte = byte_idx;
                current_width = 0;
            }
        }

        current_line.push(ch);
        current_width += ch_width;
    }

    if !current_line.is_empty() || line_start_byte < text.len() {
        lines.push(WrappedLine {
            text: current_line,
            start_byte: line_start_byte,
            end_byte: text.len(),
        });
    }

    lines
}

fn prohibited_line_start(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '、'
            | '！'
            | '？'
            | '：'
            | '；'
            | '）'
            | '】'
            | '》'
            | '〉'
            | '」'
            | '』'
            | '〕'
            | '”'
            | '’'
            | ','
            | '.'
            | '!'
            | '?'
            | ':'
            | ';'
            | ')'
            | ']'
            | '}'
    )
}

fn prohibited_line_end(ch: char) -> bool {
    matches!(
        ch,
        '（' | '【' | '《' | '〈' | '「' | '『' | '〔' | '“' | '‘' | '(' | '[' | '{'
    )
}

// ── Adaptive table rendering ──────────────────────────────────────────────

/// Result of laying out a table: the rendered grid lines plus, for each data
/// line, the row index and the byte span of each column's (padded) content
/// within that line. Border lines carry `None`. The spans let the renderer
/// highlight one cell at a time and resolve clicks to a specific cell.
struct TableRender {
    lines: Vec<String>,
    line_info: Vec<Option<TableRowInfo>>,
}

struct TableRowInfo {
    row: usize,
    /// `(byte_start, byte_end)` of each column's padded content within the
    /// line text. Length equals the column count.
    col_spans: Vec<(usize, usize)>,
}

/// Build the visual lines of a GFM-style table grid that fits within
/// `max_width` display columns. Columns are sized to their widest cell
/// (intrinsic width) when space allows; when the table would overflow,
/// columns shrink proportionally to a minimum of 3 columns and cell text
/// wraps within the allotted width.
fn build_table_render(
    headers: &[String],
    rows: &[Vec<String>],
    aligns: &[crate::document::TableAlignment],
    max_width: usize,
) -> TableRender {
    use crate::document::TableAlignment;

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
fn shrink_column_widths(intrinsic: &[usize], target: usize, min_col: usize) -> Vec<usize> {
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

fn pad_cell_text(cell: &str, width: usize, align: crate::document::TableAlignment) -> String {
    use crate::document::TableAlignment;
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

// ── End adaptive table rendering ──────────────────────────────────────────

/// Styles used during rendering.
pub struct Theme {
    pub user_fg: Color,
    pub assistant_fg: Color,
    pub error_fg: Color,
    pub system_fg: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub heading_fg: Color,
    pub quote_fg: Color,
    pub dim_fg: Color,
    pub selected_bg: Color,
    pub header_bg: Color,
    pub accent: Color,
    // opencode-style semantic design tokens.
    /// Base background painted across the entire terminal frame so the TUI
    /// owns every pixel rather than relying on the terminal emulator default.
    pub app_bg: Color,
    /// Primary foreground text.
    pub text: Color,
    /// Muted/secondary text.
    pub text_muted: Color,
    /// Solid background for panels (modals, sheets, input).
    pub panel_bg: Color,
    /// Slightly dimmer than `panel_bg`; used for sent user messages so they
    /// read as read-only compared to the live input box.
    pub user_panel_bg: Color,
    /// Slightly raised background for footer/option bars.
    pub element_bg: Color,
    /// Background for menus / suggestion popups.
    pub menu_bg: Color,
    /// Tinted band behind the user's own messages (no role label is shown).
    pub user_bg: Color,
    /// Dim overlay drawn behind modals to fake alpha.
    pub backdrop: Color,
    /// Brand / selection color.
    pub primary: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    pub border_subtle: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_fg: Color::Rgb(137, 180, 250),
            assistant_fg: Color::Rgb(205, 214, 244),
            error_fg: Color::Rgb(243, 139, 168),
            system_fg: Color::Rgb(127, 132, 156),
            code_fg: Color::Rgb(148, 226, 213),
            code_bg: Color::Rgb(22, 24, 35),
            heading_fg: Color::Rgb(94, 234, 212),
            quote_fg: Color::Rgb(249, 226, 175),
            dim_fg: Color::Rgb(127, 132, 156),
            selected_bg: Color::Rgb(30, 50, 70),
            header_bg: Color::Rgb(22, 24, 35),
            accent: Color::Rgb(94, 234, 212),
            // Cool palette: cyan / teal / sky — no purple-pink.
            app_bg: Color::Rgb(15, 16, 25),
            text: Color::Rgb(205, 214, 244),
            text_muted: Color::Rgb(122, 132, 153),
            panel_bg: Color::Rgb(22, 24, 35),
            user_panel_bg: Color::Rgb(18, 20, 30),
            element_bg: Color::Rgb(33, 37, 54),
            menu_bg: Color::Rgb(27, 30, 44),
            user_bg: Color::Rgb(29, 35, 54),
            backdrop: Color::Rgb(8, 9, 14),
            primary: Color::Rgb(34, 211, 238),
            warning: Color::Rgb(250, 204, 21),
            success: Color::Rgb(74, 222, 128),
            info: Color::Rgb(125, 211, 252),
            border_subtle: Color::Rgb(45, 50, 70),
        }
    }
}

pub struct ChatView<'a> {
    pub messages: &'a [ChatMessage],
    pub scroll: u16,
    pub selection: &'a SelectionState,
    pub current_provider: &'a str,
    pub current_model: &'a str,
    pub current_mode: AgentMode,
    pub current_goal: Option<&'a Goal>,
    /// Transient running status shown in a thin bar above the input box.
    /// Empty / "idle" / "responding" means the status bar is hidden.
    pub activity: &'a str,
    /// Spinner animation phase (cycles through braille frames while active).
    pub spinner_phase: usize,
    /// The current input-box text (masked while the API-key modal is open). The
    /// chat layout reads this so the input box can grow to fit its wrapped text.
    pub input: &'a str,
    /// When true, the header and input box are hidden (overlay modal open).
    pub chrome_hidden: bool,
    /// When set, the view is zoomed into a sub-agent task: a navigation bar is
    /// rendered and `messages` is the focused task's child stream.
    pub subagent_bar: Option<SubagentBarInfo>,
    pub theme: &'a Theme,
}

/// Info for the sub-agent navigation bar (shown when zoomed into a task).
pub struct SubagentBarInfo {
    /// Label for the focused sub-agent (its task description).
    pub label: String,
    /// 1-based index of the focused sub-agent among its siblings.
    pub index: usize,
    /// Total number of sibling sub-agent tasks.
    pub total: usize,
}

/// Layout information returned by [`draw_chat`].
pub struct ChatRender {
    /// The input box area (unchanged from before).
    pub input_rect: Rect,
    /// Total height (in lines) of the rendered message stream, ignoring the
    /// viewport clip. Used by the app loop to pin the view to the bottom.
    pub content_lines: usize,
    /// Height of the chat viewport.
    pub view_height: u16,
    /// The expanded card whose body is currently scrolled into view, so the app
    /// can render/click a sticky header pinned under the HUD bar. `None` when no
    /// expanded card body covers the top of the viewport.
    pub sticky: Option<StickyInfo>,
}

/// A sticky pinned card header (returned to the app for click handling).
pub struct StickyInfo {
    pub message_idx: usize,
    pub header: String,
    pub color: Color,
    pub block_idx: usize,
    pub rect: Rect,
    /// The content-line index of the real header inside the stream. The app
    /// uses this to re-anchor the scroll offset when the user collapses the
    /// pinned card, so the real header takes the sticky's place at the top of
    /// the viewport instead of jumping to unrelated content.
    pub header_line: usize,
}

/// Render the blocks of a single message inside the given area.
///
/// This is extracted so that normal messages and tool-step cards can share
/// the same block-rendering logic while using different containing rects.
#[allow(clippy::too_many_arguments)]
fn render_message_blocks(
    frame: &mut Frame,
    area: Rect,
    msg: &ChatMessage,
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

        match block {
            Block::Text { content } => {
                let is_user = msg.role == neenee_core::Role::User;
                let base = match msg.role {
                    neenee_core::Role::User => Style::default().fg(theme.user_fg),
                    neenee_core::Role::System => Style::default().fg(theme.system_fg),
                    _ => Style::default().fg(theme.assistant_fg),
                };
                let full_width = area.width as usize;
                let lines = if is_user {
                    wrap_text(content, area.width.saturating_sub(6) as usize)
                } else {
                    // 4-col left indent + 2-col right gutter (`CHAT_H_INSET`).
                    wrap_text(content, area.width.saturating_sub(6) as usize)
                };
                *content_lines += lines.len();

                // User messages get top/bottom padding rows (matching the input
                // box's breathing room).  The padding is a blank `user_panel_bg`
                // row with the `┃` bar so the message reads as a solid panel.
                let user_bg = theme.user_panel_bg;
                let user_content_w = full_width.saturating_sub(4);

                if is_user {
                    *content_lines += 1;
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                    } else if *current_y < area.y + area.height {
                        // Top transition: ▄ (lower half block) for both the
                        // bar (accent fg) and the panel (user_bg fg) so the
                        // half-height boundary is pixel-accurate. Box-drawing
                        // characters like ╻ are font-dependent and rarely sit
                        // at the exact 50% mark.
                        let pad = Line::from(vec![
                            Span::styled("  ", Style::default().bg(theme.app_bg)),
                            Span::styled("▄", Style::default().bg(theme.app_bg).fg(theme.accent)),
                            Span::styled(
                                "▄".repeat(user_content_w.saturating_sub(1)),
                                Style::default().fg(user_bg).bg(theme.app_bg),
                            ),
                            Span::styled("  ", Style::default().bg(theme.app_bg)),
                        ]);
                        let rect = Rect::new(area.x, *current_y, area.width, 1);
                        frame.render_widget(Paragraph::new(pad), rect);
                        *current_y += 1;
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
                        // Sent user messages share the input box's `┃` accent
                        // bar on a dimmer `user_panel_bg` band.  Selection is
                        // character-level, not line-level, so the user can
                        // highlight arbitrary substrings.
                        let bg = user_bg;
                        let text_style = Style::default().bg(bg).fg(theme.text_muted);
                        let sel_style = Style::default().bg(theme.selected_bg).fg(theme.text);
                        let sel = line_selection(sel_range, wl);

                        let mut spans = vec![
                            Span::styled("  ", Style::default().bg(theme.app_bg)),
                            Span::styled("█", Style::default().bg(bg).fg(theme.accent)),
                            Span::styled(" ", Style::default().bg(bg)),
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

                        let used = 2usize + wl.text.width();
                        spans.push(Span::styled(
                            padded_tail(user_content_w, used),
                            Style::default().bg(bg),
                        ));
                        spans.push(Span::styled("  ", Style::default().bg(theme.app_bg)));
                        Line::from(spans)
                    } else {
                        line_spans(
                            "    ",
                            Style::default(),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        )
                    };
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 4,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }

                if is_user {
                    *content_lines += 1;
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                    } else if *current_y < area.y + area.height {
                        // Bottom transition: ▀ (upper half block) for both the
                        // bar (accent fg) and the panel (user_bg fg) — matching
                        // the top transition's pixel-accurate half boundary.
                        let pad = Line::from(vec![
                            Span::styled("  ", Style::default().bg(theme.app_bg)),
                            Span::styled("▀", Style::default().bg(theme.app_bg).fg(theme.accent)),
                            Span::styled(
                                "▀".repeat(user_content_w.saturating_sub(1)),
                                Style::default().fg(user_bg).bg(theme.app_bg),
                            ),
                            Span::styled("  ", Style::default().bg(theme.app_bg)),
                        ]);
                        let rect = Rect::new(area.x, *current_y, area.width, 1);
                        frame.render_widget(Paragraph::new(pad), rect);
                        *current_y += 1;
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
                // `indent` left + 2-col right gutter (`CHAT_H_INSET`).
                let available = full_width.saturating_sub(indent + 2);
                let table = build_table_render(headers, rows, aligns, available);
                let ncols = headers.len().max(1);

                let base = Style::default().fg(theme.text);
                let border_style = Style::default().fg(theme.text_muted);
                let sel_bg = theme.selected_bg;

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
                        // tests (e.g. card headers) keep working; border rules
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
                let code_bg = theme.code_bg;
                // The solid-background band is inset from the chat edges so it
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
                                Style::default().bg(code_bg).fg(theme.dim_fg),
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
                            theme.dim_fg,
                            &wl.text,
                            line_selection(sel_range, &block_wl),
                            theme.code_fg,
                            theme.selected_bg,
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
            Block::Heading { level, content } => {
                let prefix = "   ".to_string();
                let prefix_cols = display_width_u16(&prefix);
                let modifier = if *level == 1 {
                    Modifier::BOLD | Modifier::UNDERLINED
                } else {
                    Modifier::BOLD
                };
                let style = Style::default().fg(theme.heading_fg).add_modifier(modifier);
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
                    let line = line_spans(
                        if line_index == 0 {
                            &prefix
                        } else {
                            &continuation
                        },
                        style,
                        &wl.text,
                        line_selection(sel_range, wl),
                        style,
                        theme.selected_bg,
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
            Block::Quote { content } => {
                // 5-col `▎` prefix + 2-col right gutter (`CHAT_H_INSET`).
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

                    let base = Style::default().fg(theme.quote_fg);
                    let line = line_spans(
                        "   ▎ ",
                        Style::default().fg(theme.quote_fg),
                        &wl.text,
                        line_selection(sel_range, wl),
                        base,
                        theme.selected_bg,
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
                        Line::from(vec![Span::styled(text, Style::default().fg(theme.dim_fg))]);
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
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols + 2) as usize);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
                        break;
                    }

                    let base = Style::default().fg(theme.assistant_fg);
                    let line = line_spans(
                        &prefix,
                        Style::default().fg(theme.accent),
                        &wl.text,
                        line_selection(sel_range, wl),
                        base,
                        theme.selected_bg,
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

/// Tracked info for an expanded card, used to render a sticky header pinned
/// under the HUD bar while the card's body is scrolled into view.
struct StickyCard {
    message_idx: usize,
    header: String,
    color: Color,
    /// usize::MAX for tool steps, usize::MAX - 1 for thinking cards.
    block_idx: usize,
    header_line: usize,
    body_end_line: usize,
}

/// Build the header band of an expandable card: a solid background region
/// (no border lines) reading `+ {header}` when collapsed or `- {header}` when
/// expanded, padded to the full width so it reads as a colored band. The body
/// content is expected to start at column 2 so it left-aligns with the header
/// text.
fn card_header_line(
    arrow: &str,
    header: &str,
    arrow_color: Color,
    header_color: Color,
    bg: Color,
    full_width: usize,
) -> Line<'static> {
    let lead_arrow = format!("{} ", arrow);
    let lead_header = header.to_string();
    let used = lead_arrow.width() + lead_header.width();
    Line::from(vec![
        Span::styled(
            lead_arrow,
            Style::default()
                .bg(bg)
                .fg(arrow_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            lead_header,
            Style::default()
                .bg(bg)
                .fg(header_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(padded_tail(full_width, used), Style::default().bg(bg)),
    ])
}

/// Render the shared header band of an expandable card and record its rect in
/// the layout map so clicks / `Enter` on it can toggle the card. Returns the
/// content-line index of the header (used for sticky-pin tracking). Both
/// tool-step and thinking cards delegate here.
///
/// `block_idx` is the sentinel recorded in [`BlockRegion`] so the click handler
/// can tell the two card kinds apart: `usize::MAX` for tool-step cards and
/// `usize::MAX - 1` for thinking cards.
#[allow(clippy::too_many_arguments)]
fn render_expandable_card_header(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    expanded: bool,
    marker_override: Option<&str>,
    header: &str,
    arrow_color: Color,
    header_color: Color,
    bg: Color,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) -> usize {
    let arrow = marker_override.unwrap_or(if expanded { "-" } else { "+" });
    let header_line_idx = *content_lines;

    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(card_header_line(
                arrow,
                header,
                arrow_color,
                header_color,
                bg,
                full_width,
            )),
            line_rect,
        );
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: line_rect,
        });
        *current_y += 1;
    }

    header_line_idx
}

/// Draw a single blank line padded to `full_width` with `style`'s
/// background. Used to separate sections inside an expanded card body so
/// each part gets visual breathing room.
fn draw_blank_line(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    style: Style,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(padded_tail(full_width, 0), style))),
            rect,
        );
        *current_y += 1;
    }
}

/// Draw a section label line (" Tool", " Arguments", " Result") inside an
/// expanded card body. The label sits at column 1 (one-space prefix) in
/// `label_style`; the rest of the band is filled with `pad_style`'s
/// background so it reads as a solid colored row.
#[allow(clippy::too_many_arguments)]
fn draw_section_label(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    label: &str,
    pad_style: Style,
    label_style: Style,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let indent = 2usize;
        let used = indent + label.len();
        let line = Line::from(vec![
            Span::styled(" ".repeat(indent), pad_style),
            Span::styled(label, label_style),
            Span::styled(padded_tail(full_width, used), pad_style),
        ]);
        let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tool_meta_row(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    label: &str,
    value: &str,
    pad_style: Style,
    label_style: Style,
    value_style: Style,
    label_width: usize,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let indent = 2usize;
        let gap = 2usize;
        let label_text = format!("{:<width$}", label, width = label_width);
        let used = indent + label_width + gap + value.width();
        let line = Line::from(vec![
            Span::styled(" ".repeat(indent), pad_style),
            Span::styled(label_text, label_style),
            Span::styled(" ".repeat(gap), pad_style),
            Span::styled(value.to_string(), value_style),
            Span::styled(padded_tail(full_width, used), pad_style),
        ]);
        let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }
}

/// Render a shell command line inside an expanded bash tool card. Long
/// commands wrap under the prompt so the expanded view stays compact without
/// losing the actual command that ran.
#[allow(clippy::too_many_arguments)]
fn render_meta_value_row_wrapped(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    label: &str,
    value: &str,
    pad_style: Style,
    label_style: Style,
    value_style: Style,
    label_width: usize,
    inner_w: usize,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let value = value.trim_end();
    if value.is_empty() {
        return;
    }

    let indent = 2usize;
    let gap = 2usize;
    let value_indent = indent + label_width + gap;
    let wrap_w = inner_w.max(1);
    let wrapped = wrap_text(value, wrap_w);
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
    for (idx, wl) in wrapped.iter().enumerate() {
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= chat_area.y + chat_area.height {
            break;
        }

        let mut spans = vec![Span::styled(" ".repeat(indent), pad_style)];
        if idx == 0 {
            spans.push(Span::styled(
                format!("{:<width$}", label, width = label_width),
                label_style,
            ));
            spans.push(Span::styled(" ".repeat(gap), pad_style));
        } else {
            spans.push(Span::styled(" ".repeat(label_width + gap), pad_style));
        }
        spans.push(Span::styled(wl.text.clone(), value_style));
        let used = value_indent + wl.text.width();
        spans.push(Span::styled(padded_tail(full_width, used), pad_style));
        let line = Line::from(spans);
        let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }
}

/// Render one labelled section (Arguments / a plain Result fallback) inside
/// an expanded tool-step card body. Handles scroll-skip, wrapping, semantic
/// selection layout recording, and an optional blank separator above the
/// label. Result rendering for known tools is handled by
/// [`render_tool_result_section`] instead.
#[allow(clippy::too_many_arguments)]
fn render_tool_body_section(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    label: &str,
    content: &str,
    content_style: Style,
    pad_style: Style,
    label_style: Style,
    indent: usize,
    inner_w: usize,
    separator: bool,
    code_mode: bool,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    if separator {
        draw_blank_line(
            frame,
            chat_area,
            full_width,
            pad_style,
            skip_rows,
            current_y,
            content_lines,
        );
    }

    draw_section_label(
        frame,
        chat_area,
        full_width,
        label,
        pad_style,
        label_style,
        skip_rows,
        current_y,
        content_lines,
    );

    // Content lines.
    let sel_range = block_selection_range(selection, mi, block_idx);
    let _ = sel_range;
    if code_mode {
        render_code_content(
            frame,
            chat_area,
            full_width,
            mi,
            block_idx,
            content,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            indent,
            inner_w,
        );
    } else {
        // Plain-text rendering: simple indent + wrap.
        let wrapped = wrap_text(content, inner_w);
        *content_lines += wrapped.len();
        for wl in &wrapped {
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }

            let mut line = line_spans(
                &" ".repeat(indent),
                pad_style,
                &wl.text,
                line_selection(sel_range, wl),
                content_style,
                theme.selected_bg,
            );
            let used = indent + wl.text.width();
            line.spans
                .push(Span::styled(padded_tail(full_width, used), pad_style));

            let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(Paragraph::new(line), rect);
            layout_map.push(BlockRegion {
                message_idx: mi,
                block_idx,
                start_byte: wl.start_byte,
                end_byte: wl.end_byte,
                text: wl.text.clone(),
                prefix_cols: indent as u16,
                rect,
            });
            *current_y += 1;
        }
    }
}

/// Render text content as a code block with a line-number gutter on
/// `code_bg`. Used for `read_file` / `edit_file` results and as the
/// fallback for unrecognized tools. The gutter starts at column `indent`
/// so the code aligns with the rest of the card body.
#[allow(clippy::too_many_arguments)]
fn render_code_content(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    indent: usize,
    inner_w: usize,
) {
    let code_bg = theme.code_bg;
    let mut logical_lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in content.split('\n') {
        logical_lines.push((offset, line));
        offset += line.len() + 1;
    }
    let gutter_width = logical_lines.len().to_string().len().max(2);
    let left_indent = indent;
    let gutter_gap = 1usize;
    let gutter_indent = left_indent + 1 /* space */ + gutter_width + gutter_gap;
    let wrap_width = inner_w.saturating_sub(1 + gutter_width + gutter_gap);
    let sel_range = block_selection_range(selection, mi, block_idx);

    for (line_idx, (line_start_byte, logical_line)) in logical_lines.iter().enumerate() {
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
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }

            let gutter = if wrap_idx == 0 {
                format!("{:>width$}", line_idx + 1, width = gutter_width)
            } else {
                " ".repeat(gutter_width)
            };

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
                theme.dim_fg,
                &wl.text,
                line_selection(sel_range, &block_wl),
                theme.code_fg,
                theme.selected_bg,
                full_width,
            );
            let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(Paragraph::new(line), rect);
            layout_map.push(BlockRegion {
                message_idx: mi,
                block_idx,
                start_byte: line_start_byte + wl.start_byte,
                end_byte: line_start_byte + wl.end_byte,
                text: wl.text.clone(),
                prefix_cols: gutter_indent as u16,
                rect,
            });
            *current_y += 1;
        }
    }
}

/// Render a `list_dir` / `glob` result: one entry per row on `code_bg`,
/// directories (entries ending in `/`) in `info`, files in `code_fg`. No
/// line-number gutter since listing rows have no meaningful line index.
#[allow(clippy::too_many_arguments)]
fn render_listing_content(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    indent: usize,
    inner_w: usize,
) {
    let code_bg = theme.code_bg;
    let pad = Style::default().bg(code_bg);
    let dir_fg = theme.info;
    let file_fg = theme.code_fg;
    let sel_range = block_selection_range(selection, mi, block_idx);
    let wrap_w = inner_w.saturating_sub(indent).max(1);

    let mut logical_lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in content.split('\n') {
        logical_lines.push((offset, line));
        offset += line.len() + 1;
    }

    for (line_start_byte, logical_line) in logical_lines.iter() {
        let is_dir = logical_line.ends_with('/');
        let fg = if is_dir { dir_fg } else { file_fg };
        let base = Style::default().bg(code_bg).fg(fg);
        let wrapped = wrap_text(logical_line, wrap_w);
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
        for wl in &wrapped {
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }
            let block_wl = WrappedLine {
                text: wl.text.clone(),
                start_byte: line_start_byte + wl.start_byte,
                end_byte: line_start_byte + wl.end_byte,
            };
            let mut line = line_spans(
                &" ".repeat(indent),
                pad,
                &wl.text,
                line_selection(sel_range, &block_wl),
                base,
                theme.selected_bg,
            );
            let used = indent + wl.text.width();
            line.spans
                .push(Span::styled(padded_tail(full_width, used), pad));
            let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(Paragraph::new(line), rect);
            layout_map.push(BlockRegion {
                message_idx: mi,
                block_idx,
                start_byte: line_start_byte + wl.start_byte,
                end_byte: line_start_byte + wl.end_byte,
                text: wl.text.clone(),
                prefix_cols: indent as u16,
                rect,
            });
            *current_y += 1;
        }
    }
}

/// A single logical line parsed out of grep's `path:linenum:content` format.
struct GrepLine<'a> {
    path: &'a str,
    lineno: &'a str,
    content: &'a str,
    /// Byte offset of `content` within the original ripgrep output line.
    content_offset: usize,
}

/// Parse `path:linenum:content` (ripgrep's default with `-n`). Paths may
/// contain `:` (e.g. Windows `C:\foo`), so the scan accepts the first colon
/// that is followed by an all-digit run and another colon as the
/// line-number separator. Returns `None` for blank separators or any line
/// that doesn't match the ripgrep shape.
fn parse_grep_line(line: &str) -> Option<GrepLine<'_>> {
    for (idx, ch) in line.char_indices() {
        if ch != ':' {
            continue;
        }
        let after = &line[idx + 1..];
        let digits_end = after
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit())
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if digits_end > 0 && after.as_bytes().get(digits_end) == Some(&b':') {
            let path = &line[..idx];
            if path.is_empty() {
                continue;
            }
            let lineno = &after[..digits_end];
            let content = &after[digits_end + 1..];
            let content_offset = idx + 1 + digits_end + 1;
            return Some(GrepLine {
                path,
                lineno,
                content,
                content_offset,
            });
        }
    }
    None
}

/// Emit `text` as one or more wrapped rows at column `indent`, all styled
/// with `style` on `pad`'s background, recording a selectable [`BlockRegion`]
/// per row whose byte range is anchored at `abs_start` within the tool
/// output. Used for grep path headers, ripgrep separator rows, and any
/// other "simple" result row that doesn't need a line-number gutter.
#[allow(clippy::too_many_arguments)]
fn emit_simple_rows(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    indent: usize,
    text: &str,
    abs_start: usize,
    pad: Style,
    style: Style,
    sel_range: Option<(usize, Option<usize>)>,
    selected_bg: Color,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let wrap_w = full_width.saturating_sub(indent).max(1);
    let wrapped = wrap_text(text, wrap_w);
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
    for wl in &wrapped {
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= chat_area.y + chat_area.height {
            break;
        }
        let block_wl = WrappedLine {
            text: wl.text.clone(),
            start_byte: abs_start + wl.start_byte,
            end_byte: abs_start + wl.end_byte,
        };
        let mut line = line_spans(
            &" ".repeat(indent),
            pad,
            &wl.text,
            line_selection(sel_range, &block_wl),
            style,
            selected_bg,
        );
        let used = indent + wl.text.width();
        line.spans
            .push(Span::styled(padded_tail(full_width, used), pad));
        let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx,
            start_byte: block_wl.start_byte,
            end_byte: block_wl.end_byte,
            text: wl.text.clone(),
            prefix_cols: indent as u16,
            rect,
        });
        *current_y += 1;
    }
}

/// Render a `grep` result by grouping matches under their file path. Each
/// new path is printed once as a bold `heading_fg` header row; each match
/// is shown as `{lineno}  {content}` with the line number dimmed and the
/// line-number column aligned across the whole result. Non-match lines
/// (ripgrep block separators, etc.) fall back to a dimmed plain row.
/// Selection byte ranges are anchored in the original tool output so
/// copy/cut works across the visible match content.
#[allow(clippy::too_many_arguments)]
fn render_grep_content(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    indent: usize,
    inner_w: usize,
) {
    let code_bg = theme.code_bg;
    let pad = Style::default().bg(code_bg);
    let header_style = Style::default()
        .bg(code_bg)
        .fg(theme.heading_fg)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().bg(code_bg).fg(theme.dim_fg);
    let match_style = Style::default().bg(code_bg).fg(theme.code_fg);
    let sel_range = block_selection_range(selection, mi, block_idx);

    // Walk logical lines with their byte offsets in `content`.
    let mut logical: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in content.split('\n') {
        logical.push((offset, line));
        offset += line.len() + 1;
    }

    // Width of the line-number column: the widest lineno across all matches,
    // so the content column stays aligned within and across files.
    let mut lineno_width = 1usize;
    for (_, line) in &logical {
        if let Some(p) = parse_grep_line(line) {
            lineno_width = lineno_width.max(p.lineno.len());
        }
    }
    let gap = 2usize;
    let content_cols = indent + lineno_width + gap;
    let content_wrap_w = inner_w.saturating_sub(lineno_width + gap).max(1);

    let mut current_path: Option<&str> = None;

    for (line_start_byte, logical_line) in &logical {
        match parse_grep_line(logical_line) {
            Some(parsed) => {
                if current_path != Some(parsed.path) {
                    current_path = Some(parsed.path);
                    emit_simple_rows(
                        frame,
                        chat_area,
                        full_width,
                        mi,
                        block_idx,
                        indent,
                        parsed.path,
                        *line_start_byte,
                        pad,
                        header_style,
                        sel_range,
                        theme.selected_bg,
                        layout_map,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                }
                // Absolute byte offset of `content` within the tool output.
                let content_abs = line_start_byte + parsed.content_offset;
                let wrapped = wrap_text(parsed.content, content_wrap_w);
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
                    if *current_y >= chat_area.y + chat_area.height {
                        break;
                    }
                    let lineno_span = if wrap_idx == 0 {
                        let lpad = lineno_width.saturating_sub(parsed.lineno.len());
                        Span::styled(format!("{}{}", " ".repeat(lpad), parsed.lineno), dim)
                    } else {
                        Span::styled(" ".repeat(lineno_width), dim)
                    };
                    let block_wl = WrappedLine {
                        text: wl.text.clone(),
                        start_byte: content_abs + wl.start_byte,
                        end_byte: content_abs + wl.end_byte,
                    };
                    let selected = line_selection(sel_range, &block_wl);
                    let mut spans = vec![
                        Span::styled(" ".repeat(indent), pad),
                        lineno_span,
                        Span::styled(" ".repeat(gap), pad),
                    ];
                    match selected {
                        None => spans.push(Span::styled(wl.text.clone(), match_style)),
                        Some((lo, hi)) => {
                            if lo > 0 {
                                spans.push(Span::styled(wl.text[..lo].to_string(), match_style));
                            }
                            spans.push(Span::styled(
                                wl.text[lo..hi].to_string(),
                                match_style.bg(theme.selected_bg),
                            ));
                            if hi < wl.text.len() {
                                spans.push(Span::styled(wl.text[hi..].to_string(), match_style));
                            }
                        }
                    }
                    let used = content_cols + wl.text.width();
                    spans.push(Span::styled(padded_tail(full_width, used), pad));
                    let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
                    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
                    layout_map.push(BlockRegion {
                        message_idx: mi,
                        block_idx,
                        start_byte: block_wl.start_byte,
                        end_byte: block_wl.end_byte,
                        text: wl.text.clone(),
                        prefix_cols: content_cols as u16,
                        rect,
                    });
                    *current_y += 1;
                }
            }
            None => {
                emit_simple_rows(
                    frame,
                    chat_area,
                    full_width,
                    mi,
                    block_idx,
                    indent,
                    logical_line,
                    *line_start_byte,
                    pad,
                    dim,
                    sel_range,
                    theme.selected_bg,
                    layout_map,
                    skip_rows,
                    current_y,
                    content_lines,
                );
            }
        }
    }
}

/// Render a `bash` result: plain wrapped rows on `code_bg` with no
/// line-number gutter (command output rows have no meaningful line index).
/// Section markers emitted by the tool (`Exit N`, `STDOUT:`, `STDERR:`,
/// `(success, stderr):`, `[Output truncated`, `[Output was large`) are
/// highlighted in `warning` so they stand out from the output itself.
#[allow(clippy::too_many_arguments)]
fn render_bash_content(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    indent: usize,
    inner_w: usize,
) {
    let result_bg = theme.menu_bg;
    let pad = Style::default().bg(result_bg);
    let base = Style::default().bg(result_bg).fg(theme.code_fg);
    let marker_style = Style::default()
        .bg(result_bg)
        .fg(theme.warning)
        .add_modifier(Modifier::BOLD);
    let content = content.trim_end_matches(&['\r', '\n'][..]);
    if content.is_empty() {
        return;
    }
    let sel_range = block_selection_range(selection, mi, block_idx);
    let wrap_w = inner_w.saturating_sub(indent).max(1);

    let mut logical_lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in content.split('\n') {
        logical_lines.push((offset, line));
        offset += line.len() + 1;
    }

    for (line_start_byte, logical_line) in logical_lines.iter() {
        let trimmed = logical_line.trim_end();
        let is_marker = trimmed.starts_with("Exit ")
            || trimmed == "STDOUT:"
            || trimmed == "STDERR:"
            || trimmed.starts_with("(success, stderr):")
            || trimmed.starts_with("[Output truncated")
            || trimmed.starts_with("[Output was large");

        let style = if is_marker { marker_style } else { base };
        let wrapped = wrap_text(logical_line, wrap_w);
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
        for wl in &wrapped {
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }
            let block_wl = WrappedLine {
                text: wl.text.clone(),
                start_byte: line_start_byte + wl.start_byte,
                end_byte: line_start_byte + wl.end_byte,
            };
            let mut line = line_spans(
                &" ".repeat(indent),
                pad,
                &wl.text,
                line_selection(sel_range, &block_wl),
                style,
                theme.selected_bg,
            );
            let used = indent + wl.text.width();
            line.spans
                .push(Span::styled(padded_tail(full_width, used), pad));
            let rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(Paragraph::new(line), rect);
            layout_map.push(BlockRegion {
                message_idx: mi,
                block_idx,
                start_byte: line_start_byte + wl.start_byte,
                end_byte: line_start_byte + wl.end_byte,
                text: wl.text.clone(),
                prefix_cols: indent as u16,
                rect,
            });
            *current_y += 1;
        }
    }
}

/// Render the "Result" section of an expanded tool-step card. Draws the
/// blank separator and `Result` label on the surrounding body background
/// (so the label aligns with `Tool` / `Arguments`), then dispatches the
/// content rendering based on the tool name. Known tools with structured
/// output get a specialized renderer; everything else falls back to a
/// line-numbered code block via [`render_code_content`].
#[allow(clippy::too_many_arguments)]
fn render_tool_result_section(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    name: &str,
    output: &str,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    indent: usize,
    inner_w: usize,
    separator: bool,
    body_pad: Style,
    body_label: Style,
) {
    if separator {
        draw_blank_line(
            frame,
            chat_area,
            full_width,
            body_pad,
            skip_rows,
            current_y,
            content_lines,
        );
    }
    draw_section_label(
        frame,
        chat_area,
        full_width,
        "Result",
        body_pad,
        body_label,
        skip_rows,
        current_y,
        content_lines,
    );

    let block_idx = 1usize;
    match name {
        "list_dir" | "glob" => render_listing_content(
            frame,
            chat_area,
            full_width,
            mi,
            block_idx,
            output,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            indent,
            inner_w,
        ),
        "grep" => render_grep_content(
            frame,
            chat_area,
            full_width,
            mi,
            block_idx,
            output,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            indent,
            inner_w,
        ),
        "bash" => render_bash_content(
            frame,
            chat_area,
            full_width,
            mi,
            block_idx,
            output,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            indent + 2,
            inner_w.saturating_sub(2),
        ),
        _ => render_code_content(
            frame,
            chat_area,
            full_width,
            mi,
            block_idx,
            output,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            indent,
            inner_w,
        ),
    }
}

/// Render a sub-agent `task` tool step as a compact, non-expandable card.
/// Activating it (click / Enter) navigates into a dedicated sub-agent view
/// rather than expanding a body inline. The card shows a one-line header
/// (the task description + duration) and a live status line summarizing the
/// sub-agent's progress.
#[allow(clippy::too_many_arguments)]
fn render_subagent_inline_card(
    frame: &mut Frame,
    chat_area: Rect,
    msg: &ChatMessage,
    mi: usize,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };

    let (status_color, done) = match &msg.kind {
        crate::document::MessageKind::ToolStep {
            output: Some(output),
            ..
        } if output.starts_with("Error") => (theme.error_fg, true),
        crate::document::MessageKind::ToolStep {
            output: Some(_), ..
        } => (theme.success, true),
        _ => (theme.info, false),
    };

    let chat_area = chat_band_rect(chat_area);
    let full_width = chat_area.width as usize;
    if full_width < 8 {
        return;
    }

    let bg = theme.element_bg;
    let marker = if done { "✓" } else { "▸" };

    // Header band: marker + header text, registered as a tool-step card header
    // (block_idx = usize::MAX) so the existing click/Enter handling recognizes
    // it; the app decides to navigate rather than toggle for `task` steps.
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(card_header_line(
                marker,
                &header,
                status_color,
                theme.text,
                bg,
                full_width,
            )),
            line_rect,
        );
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: line_rect,
        });
        *current_y += 1;
    }

    // Live status line (e.g. "↳ Running: grep foo" / "↳ Completed · 3 calls").
    if let Some(status) = msg.subagent_status_line() {
        let inner_width = full_width.saturating_sub(2);
        let wrapped = wrap_text(&status, inner_width.max(1));
        *content_lines += wrapped.len();
        for wl in &wrapped {
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }
            let used = 2 + wl.text.width();
            let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  ", Style::default().bg(bg)),
                    Span::styled(
                        wl.text.clone(),
                        Style::default().bg(bg).fg(theme.text_muted),
                    ),
                    Span::styled(padded_tail(full_width, used), Style::default().bg(bg)),
                ])),
                line_rect,
            );
            // Make the whole status line part of the same clickable header so
            // clicking anywhere on the card enters the sub-agent view.
            layout_map.push(BlockRegion {
                message_idx: mi,
                block_idx: usize::MAX,
                start_byte: 0,
                end_byte: 0,
                text: String::new(),
                prefix_cols: 0,
                rect: line_rect,
            });
            *current_y += 1;
        }
    }
}

/// Render the sub-agent navigation bar: the focused task's label + position
/// among siblings on the left, and the return / cycle-sibling hints on the
/// right. Drawn across the full chat width inside the app_bg gutters.
fn draw_subagent_bar(frame: &mut Frame, rect: Rect, bar: &SubagentBarInfo, theme: &Theme) {
    let band = chat_band_rect(rect);
    let full_width = band.width as usize;
    if full_width < 8 {
        return;
    }
    let bg = theme.menu_bg;
    let muted = Style::default().bg(bg).fg(theme.text_muted);
    let label_style = Style::default()
        .bg(bg)
        .fg(theme.text)
        .add_modifier(Modifier::BOLD);
    let accent = Style::default().bg(bg).fg(theme.accent);

    let left_label = format!(" {} ", "Subagent");
    let desc = bar.label.to_string();
    let count = if bar.total > 1 {
        format!(" ({} of {}) ", bar.index, bar.total)
    } else {
        " ".to_string()
    };
    let right = "Esc back   [ prev   ] next ".to_string();

    let left_used = left_label.width() + desc.width() + count.width();
    let gap = full_width.saturating_sub(left_used + right.width());
    let mut spans = vec![
        Span::styled(left_label, label_style),
        Span::styled(desc, accent),
        Span::styled(count, muted),
        Span::styled(" ".repeat(gap), Style::default().bg(bg)),
        Span::styled(right, muted),
    ];
    let used: usize = spans.iter().map(|s| s.width()).sum();
    if used < full_width {
        spans.push(Span::styled(
            " ".repeat(full_width - used),
            Style::default().bg(bg),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), band);
}

/// Render a tool-step message as a card with a summary header,
/// expandable body, and per-line scroll handling so tall cards scroll like
/// normal messages.
#[allow(clippy::too_many_arguments)]
fn render_tool_step_card(
    frame: &mut Frame,
    chat_area: Rect,
    msg: &ChatMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    sticky_cards: &mut Vec<StickyCard>,
    spinner_phase: usize,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };

    let status_color = match &msg.kind {
        crate::document::MessageKind::ToolStep {
            output: Some(output),
            ..
        } if output.starts_with("Error") => theme.error_fg,
        crate::document::MessageKind::ToolStep {
            output: Some(_), ..
        } => theme.success,
        _ => theme.info,
    };
    let running = matches!(
        &msg.kind,
        crate::document::MessageKind::ToolStep { output: None, .. }
    );

    let expanded = msg.tool_step_expanded() == Some(true);
    // Render into the inset band so the `element_bg`/`menu_bg`/`code_bg` bands
    // never touch the terminal frame — they sit inside the uniform 2-cell
    // `app_bg` gutters shared with user panels and code blocks. All helpers
    // below (header, body sections, child tool steps) read `chat_area.x` /
    // `chat_area.width` directly, so shrinking here propagates everywhere.
    let chat_area = chat_band_rect(chat_area);
    let full_width = chat_area.width as usize;
    if full_width < 8 {
        // Too narrow to draw a card; fall back to plain block rendering.
        render_message_blocks(
            frame,
            chat_area,
            msg,
            mi,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            true,
        );
        return;
    }

    // Header band: solid background region with a `+`/`-` expand marker (no
    // border lines). Delegated to the shared expandable-card header helper so
    // tool-step and thinking cards stay in sync. `inner_width` is the full
    // band width; each body section subtracts its own indent before wrapping.
    let inner_width = chat_area.width as usize;
    let header_line_idx = render_expandable_card_header(
        frame,
        chat_area,
        full_width,
        mi,
        usize::MAX,
        expanded,
        running.then(|| spinner_frame(spinner_phase)),
        &header,
        status_color,
        theme.text_muted,
        theme.element_bg,
        layout_map,
        skip_rows,
        current_y,
        content_lines,
    );

    // Bash-only collapsed preview: under the header, show the `$ command` line
    // and a truncated (ANSI-stripped) excerpt of the output so the user can
    // see what ran and roughly what it produced without expanding. Other tool
    // types keep the all-or-nothing collapsed header. Expand for the full
    // structured view (Tool / Arguments / Result with a code gutter).
    if !expanded {
        if let crate::document::MessageKind::ToolStep {
            name,
            arguments,
            output: Some(output),
            ..
        } = &msg.kind
        {
            if name == "bash" && !output.trim().is_empty() {
                render_bash_preview(
                    frame,
                    chat_area,
                    full_width,
                    mi,
                    arguments,
                    output,
                    theme,
                    layout_map,
                    skip_rows,
                    current_y,
                    content_lines,
                );
            }
        }
    }

    // Body region (only when expanded; collapsed cards show just the header band).
    // Body content is indented 2 cols so it left-aligns with the header text in
    // `+ {header}` (the `+` sits at col 0, the header text at col 2). A blank
    // `menu_bg` row separates the header from the body and every pair of
    // sections (Tool / Arguments / Result / children) so each part breathes.
    if expanded {
        let body_bg = theme.menu_bg;
        let pad = Style::default().bg(body_bg);
        let label_style = Style::default()
            .bg(body_bg)
            .fg(theme.text_muted)
            .add_modifier(Modifier::BOLD);
        let arg_style = Style::default().bg(body_bg).fg(theme.text_muted);
        let indent = 2usize;
        let inner_w = inner_width.saturating_sub(indent);
        let meta_label_width = 9usize;
        let meta_label_style = Style::default()
            .bg(body_bg)
            .fg(theme.text_muted)
            .add_modifier(Modifier::BOLD);
        let meta_value_style = Style::default().bg(body_bg).fg(theme.dim_fg);
        let command_style = Style::default().bg(body_bg).fg(theme.text);

        draw_blank_line(
            frame,
            chat_area,
            full_width,
            pad,
            skip_rows,
            current_y,
            content_lines,
        );

        if let crate::document::MessageKind::ToolStep {
            name,
            arguments,
            output,
            ..
        } = &msg.kind
        {
            let kv = crate::document::parse_arguments_kv(arguments);
            if name == "bash" {
                let command = kv
                    .iter()
                    .find(|(k, _)| k == "command")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or_default();
                draw_tool_meta_row(
                    frame,
                    chat_area,
                    full_width,
                    "Tool",
                    name,
                    pad,
                    meta_label_style,
                    meta_value_style,
                    meta_label_width,
                    skip_rows,
                    current_y,
                    content_lines,
                );
                render_meta_value_row_wrapped(
                    frame,
                    chat_area,
                    full_width,
                    "Arguments",
                    command,
                    pad,
                    meta_label_style,
                    command_style,
                    meta_label_width,
                    full_width.saturating_sub(indent + meta_label_width + 2),
                    skip_rows,
                    current_y,
                    content_lines,
                );
                if let Some(output_str) = output {
                    if !output_str.is_empty() {
                        render_tool_result_section(
                            frame,
                            chat_area,
                            full_width,
                            mi,
                            name,
                            output_str,
                            selection,
                            theme,
                            layout_map,
                            skip_rows,
                            current_y,
                            content_lines,
                            indent,
                            inner_w,
                            false,
                            pad,
                            label_style,
                        );
                    }
                }
            } else {
                draw_tool_meta_row(
                    frame,
                    chat_area,
                    full_width,
                    "Tool",
                    name,
                    pad,
                    meta_label_style,
                    meta_value_style,
                    meta_label_width,
                    skip_rows,
                    current_y,
                    content_lines,
                );

                let display_args: String = kv
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !display_args.is_empty() {
                    render_tool_body_section(
                        frame,
                        chat_area,
                        full_width,
                        mi,
                        0,
                        "Arguments",
                        &display_args,
                        arg_style,
                        pad,
                        label_style,
                        indent,
                        inner_w,
                        false,
                        false,
                        selection,
                        theme,
                        layout_map,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                }

                // ── Result ── (only when output exists). Dispatched per tool
                // name so listings, grep matches, and command output each get a
                // purpose-built renderer; unknown tools fall back to a
                // line-numbered code block. The label sits on the body
                // background while only the result content uses `code_bg`.
                if let Some(output_str) = output {
                    if !output_str.is_empty() {
                        render_tool_result_section(
                            frame,
                            chat_area,
                            full_width,
                            mi,
                            name,
                            output_str,
                            selection,
                            theme,
                            layout_map,
                            skip_rows,
                            current_y,
                            content_lines,
                            indent,
                            inner_w,
                            false,
                            pad,
                            label_style,
                        );
                    }
                }
            }
        }

        // ── Nested sub-agent children ── (blank-separated from Result).
        if let crate::document::MessageKind::ToolStep { children, .. } = &msg.kind {
            if !children.is_empty() {
                draw_blank_line(
                    frame,
                    chat_area,
                    full_width,
                    pad,
                    skip_rows,
                    current_y,
                    content_lines,
                );
            }
            for child in children {
                if child.is_tool_step() {
                    render_child_tool_step(
                        frame,
                        chat_area,
                        child,
                        status_color,
                        theme,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                } else {
                    let remaining_height = chat_area
                        .y
                        .saturating_add(chat_area.height)
                        .saturating_sub(*current_y);
                    let child_area = Rect::new(
                        chat_area.x + 6,
                        *current_y,
                        chat_area.width.saturating_sub(12),
                        remaining_height,
                    );
                    render_message_blocks(
                        frame,
                        child_area,
                        child,
                        usize::MAX,
                        selection,
                        theme,
                        layout_map,
                        skip_rows,
                        current_y,
                        content_lines,
                        false,
                    );
                }
            }
        }

        draw_blank_line(
            frame,
            chat_area,
            full_width,
            pad,
            skip_rows,
            current_y,
            content_lines,
        );
    }

    // Bottom border.
    let _ = status_color;
    if expanded {
        sticky_cards.push(StickyCard {
            message_idx: mi,
            header,
            color: match &msg.kind {
                crate::document::MessageKind::ToolStep {
                    output: Some(o), ..
                } if o.starts_with("Error") => theme.error_fg,
                crate::document::MessageKind::ToolStep {
                    output: Some(_), ..
                } => theme.success,
                _ => theme.info,
            },
            block_idx: usize::MAX,
            header_line: header_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// Render a nested child tool step as a compact header line plus its output.
#[allow(clippy::too_many_arguments)]
fn render_child_tool_step(
    frame: &mut Frame,
    chat_area: Rect,
    child: &ChatMessage,
    status_color: Color,
    theme: &Theme,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let Some(header) = child.tool_step_header() else {
        return;
    };
    let body_bg = theme.menu_bg;
    let full_width = chat_area.width as usize;
    let indent = 6usize;

    let header_text = format!("⚒ {}", header);
    let header_lines = wrap_text(&header_text, full_width.saturating_sub(indent));
    *content_lines += header_lines.len();
    for wl in &header_lines {
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= chat_area.y + chat_area.height {
            break;
        }
        let used = indent + wl.text.width();
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ".repeat(indent), Style::default().bg(body_bg)),
                Span::styled(
                    wl.text.clone(),
                    Style::default().bg(body_bg).fg(status_color),
                ),
                Span::styled(padded_tail(full_width, used), Style::default().bg(body_bg)),
            ])),
            line_rect,
        );
        *current_y += 1;
    }

    if let crate::document::MessageKind::ToolStep {
        output: Some(output),
        ..
    } = &child.kind
    {
        let output_lines = wrap_text(output, full_width.saturating_sub(indent + 1));
        *content_lines += output_lines.len();
        for wl in &output_lines {
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }
            let used = indent + wl.text.width();
            let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ".repeat(indent), Style::default().bg(body_bg)),
                    Span::styled(
                        wl.text.clone(),
                        Style::default().bg(body_bg).fg(theme.assistant_fg),
                    ),
                    Span::styled(padded_tail(full_width, used), Style::default().bg(body_bg)),
                ])),
                line_rect,
            );
            *current_y += 1;
        }
    }
}

/// Render a thinking/reasoning message as an expandable card. Shares its
/// header band with tool-step cards via `render_expandable_card_header`.
#[allow(clippy::too_many_arguments)]
fn render_thinking_card(
    frame: &mut Frame,
    chat_area: Rect,
    msg: &ChatMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    sticky_cards: &mut Vec<StickyCard>,
    spinner_phase: usize,
) {
    let Some(header) = msg.thinking_header() else {
        return;
    };
    let expanded = msg.thinking_expanded() == Some(true);
    let running = matches!(
        &msg.kind,
        crate::document::MessageKind::Thinking {
            duration_ms: None,
            ..
        }
    );
    // Render into the inset band so the `element_bg`/`menu_bg` bands sit inside
    // the uniform 2-cell `app_bg` gutters, matching user panels and code blocks.
    let chat_area = chat_band_rect(chat_area);
    let full_width = chat_area.width as usize;

    // Too narrow to render a padded region — fall back to plain blocks.
    if full_width < 8 {
        render_message_blocks(
            frame,
            chat_area,
            msg,
            mi,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            true,
        );
        return;
    }

    // Header band: shared expandable-card header (`+ {header}` / `- {header}`).
    let header_line_idx = render_expandable_card_header(
        frame,
        chat_area,
        full_width,
        mi,
        usize::MAX - 1,
        expanded,
        running.then(|| spinner_frame(spinner_phase)),
        &header,
        theme.info,
        theme.text_muted,
        theme.element_bg,
        layout_map,
        skip_rows,
        current_y,
        content_lines,
    );

    // Body region: a subtly different background band, shown only when expanded.
    // Body content is indented 2 cols so it left-aligns with the header text in
    // `+ {header}` (the `+` sits at col 0, the header text at col 2). A blank
    // `menu_bg` row after the header and between consecutive text blocks gives
    // the reasoning room to breathe (paragraph breaks inside a single block are
    // already preserved as empty rows by `wrap_text`).
    if expanded {
        let body_bg = theme.menu_bg;
        let pad = Style::default().bg(body_bg);
        let indent = 2usize;
        let inner_width = full_width.saturating_sub(indent);
        // Blank line after the header band — mirrors the tool-step card so
        // both expandable cards open with the same breathing room.
        draw_blank_line(
            frame,
            chat_area,
            full_width,
            pad,
            skip_rows,
            current_y,
            content_lines,
        );
        let mut emitted_any_block = false;
        for (bi, block) in msg.blocks.iter().enumerate() {
            if let Block::Text { content } = block {
                if emitted_any_block {
                    draw_blank_line(
                        frame,
                        chat_area,
                        full_width,
                        pad,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                }
                emitted_any_block = true;
                let lines = wrap_text(content, inner_width);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= chat_area.y + chat_area.height {
                        break;
                    }
                    let sel_range = block_selection_range(selection, mi, bi);
                    let selected = matches!(line_selection(sel_range, wl), Some((s, e)) if s != e);
                    let used = indent + wl.text.width();
                    let line = Line::from(vec![
                        Span::styled(" ".repeat(indent), pad),
                        Span::styled(
                            wl.text.clone(),
                            Style::default()
                                .bg(if selected { theme.selected_bg } else { body_bg })
                                .fg(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                }),
                        ),
                        Span::styled(padded_tail(full_width, used), pad),
                    ]);
                    let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);
                    layout_map.push(BlockRegion {
                        message_idx: mi,
                        block_idx: bi,
                        start_byte: wl.start_byte,
                        end_byte: wl.end_byte,
                        text: wl.text.clone(),
                        prefix_cols: indent as u16,
                        rect: line_rect,
                    });
                    *current_y += 1;
                }
            }
        }
        draw_blank_line(
            frame,
            chat_area,
            full_width,
            pad,
            skip_rows,
            current_y,
            content_lines,
        );
    }

    if expanded {
        sticky_cards.push(StickyCard {
            message_idx: mi,
            header,
            color: theme.text_muted,
            block_idx: usize::MAX - 1,
            header_line: header_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// Produce a run of spaces that fills the rest of a full-width line so a
/// region reads as a solid colored band (the caller attaches the bg style).
fn padded_tail(full_width: usize, used: usize) -> String {
    " ".repeat(full_width.saturating_sub(used))
}

/// Strip ANSI CSI escape sequences (color/style codes) from a string. Command
/// output often carries terminal color codes that would render as garbage in
/// the TUI, so the bash preview runs output through this before truncating.
/// Handles the common `\x1b[...m` color sequences; other escape types are
/// rare in command output and left untouched.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Maximum number of output lines shown in the collapsed bash preview before
/// the `…` truncation marker kicks in.
const BASH_PREVIEW_LINES: usize = 8;

/// Render a compact, truncated preview of a completed bash call under the
/// collapsed card header: a `$ <command>` line and the first
/// `BASH_PREVIEW_LINES` lines of ANSI-stripped output (with a `…` marker
/// when there is more). Each rendered row is registered with `block_idx =
/// usize::MAX` so clicking anywhere on the preview toggles the card open,
/// matching the expandable-card interaction model.
#[allow(clippy::too_many_arguments)]
fn render_bash_preview(
    frame: &mut Frame,
    chat_area: Rect,
    full_width: usize,
    mi: usize,
    arguments: &str,
    output: &str,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let body_bg = theme.menu_bg;
    let pad = Style::default().bg(body_bg);
    let indent = 2usize;
    let inner_w = full_width.saturating_sub(indent).max(1);

    // Extract the command from the JSON arguments, keep only its first line.
    let command = crate::document::parse_arguments_kv(arguments)
        .into_iter()
        .find(|(k, _)| k == "command")
        .map(|(_, v)| v)
        .unwrap_or_default();
    let command_first = command.lines().next().unwrap_or(&command);

    // Build the preview rows: the `$ command` line, then the visible output
    // lines, then a `…` marker when the output was truncated.
    let clean_output = strip_ansi(output);
    let out_lines: Vec<&str> = clean_output.lines().collect();
    let truncated = out_lines.len() > BASH_PREVIEW_LINES;
    let visible: Vec<&str> = out_lines.into_iter().take(BASH_PREVIEW_LINES).collect();

    // Each entry is (text, fg color). Lines are hard-truncated to the inner
    // width (no per-line ellipsis) so the preview height stays predictable;
    // the trailing `…` row already signals "more output below".
    let mut rows: Vec<(String, Color)> = Vec::with_capacity(2 + visible.len());
    let cmd_max = inner_w.saturating_sub(2); // reserve `$ `
    let cmd_first_trunc: String = command_first.chars().take(cmd_max).collect();
    rows.push((format!("$ {}", cmd_first_trunc), theme.text));
    for line in &visible {
        let trunc: String = line.chars().take(inner_w).collect();
        rows.push((trunc, theme.text_muted));
    }
    if truncated {
        rows.push(("…".to_string(), theme.dim_fg));
    }

    for (text, fg) in &rows {
        *content_lines += 1;
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= chat_area.y + chat_area.height {
            break;
        }
        let used = indent + text.width();
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ".repeat(indent), pad),
                Span::styled(text.clone(), Style::default().bg(body_bg).fg(*fg)),
                Span::styled(padded_tail(full_width, used), pad),
            ])),
            line_rect,
        );
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: line_rect,
        });
        *current_y += 1;
    }
}

/// Draw the main chat area, recording layout info.
pub fn draw_chat(frame: &mut Frame, layout_map: &mut LayoutMap, view: ChatView<'_>) -> ChatRender {
    let ChatView {
        messages,
        scroll,
        selection,
        current_provider: _,
        current_model,
        current_mode: _,
        current_goal,
        activity,
        spinner_phase,
        input,
        chrome_hidden,
        subagent_bar,
        theme,
    } = view;
    let full = frame.size();
    // Components render inside the vertical viewport margins (1 cell top and
    // bottom); only the background fill uses the full terminal rect.
    let size = viewport_rect(frame);

    // Paint the entire frame with the app background so the TUI owns every
    // pixel rather than leaving gaps at the terminal emulator's default color.
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.app_bg)),
        full,
    );

    let checklist = current_goal.and_then(goal_checklist_summary);
    // +1 for the thin separator rule drawn beneath the header content.
    // Hidden entirely when an overlay modal is open (chrome_hidden).
    let header_height: u16 = if chrome_hidden {
        0
    } else if checklist.is_some() {
        3
    } else {
        2
    };
    // The status bar (animated spinner + activity text) sits on its own line
    // directly above the input box. It is shown only for non-streaming,
    // non-idle activity so the chat reclaims that row when nothing is running.
    let status_active =
        !chrome_hidden && !activity.is_empty() && activity != "idle" && activity != "responding";
    let status_height: u16 = if status_active { 1 } else { 0 };

    // The input box grows with its content: the typed text wraps onto new
    // lines and the box expands to fit, up to roughly half the terminal so the
    // chat history always stays visible. The inner text width reserves the
    // thick left bar and a leading padding space.
    let input_text_width = (size.width as usize).saturating_sub(6).max(1);
    let input_wrapped_lines = wrap_text(input, input_text_width).len().max(1);
    let desired_input_height = input_wrapped_lines as u16 + 2; // top/bottom padding rows
    let max_input_height = (size.height / 2).max(3);
    let input_box_height = desired_input_height.min(max_input_height);
    let footer_height: u16 = if chrome_hidden {
        0
    } else {
        status_height + input_box_height + 1 // + hint line (bottom spacing comes from the viewport margin)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height), // Header and optional checklist dock
            Constraint::Min(0),                // Chat
            Constraint::Length(footer_height), // Status? + input box + hint line + bottom gap
        ])
        .split(size);

    // 1. Header — just the model name, plus optional goal.
    //    Skipped entirely when an overlay modal is open.
    if !chrome_hidden {
        let goal = current_goal.map(|goal| {
            let objective = goal.objective.chars().take(32).collect::<String>();
            let suffix = if goal.objective.chars().count() > 32 {
                "..."
            } else {
                ""
            };
            let mark = if goal.status == GoalStatus::Complete {
                "✓"
            } else {
                "◎"
            };
            let progress = checklist
                .as_ref()
                .map(|(done, total, _)| format!(" [{}/{}]", done, total))
                .unwrap_or_default();
            format!("{} {}{}{}", mark, objective, suffix, progress)
        });
        let mut header_spans = vec![
            Span::raw(" "),
            Span::styled(
                current_model.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(goal) = goal {
            header_spans.push(Span::raw("   "));
            header_spans.push(Span::styled(goal, Style::default().fg(theme.text_muted)));
        }
        let mut header_lines = vec![Line::from(header_spans)];
        if let Some((done, total, current)) = checklist {
            header_lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("Tasks {}/{}  ", done, total),
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(current, Style::default().fg(theme.text_muted)),
            ]));
        }
        // Header content with a thin separator rule along the bottom edge.
        let header_block = RtBlock::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border_subtle));
        frame.render_widget(Paragraph::new(header_lines).block(header_block), chunks[0]);
    } // end !chrome_hidden

    // 2. Chat History
    // When zoomed into a sub-agent, reserve a 1-line navigation band at the
    // bottom of the chat viewport for the sub-agent bar.
    let (chat_area, subagent_bar_rect) = if subagent_bar.is_some() {
        let sub = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(chunks[1]);
        (sub[0], Some(sub[1]))
    } else {
        (chunks[1], None)
    };
    let mut current_y = chat_area.y;
    // Account for scroll
    let mut skip_rows = scroll as usize;
    // Total stream height, counted independently of the viewport clip so the
    // app loop can follow the bottom.
    let mut content_lines: usize = 0;
    // Expanded cards collected during the pass, for the sticky pinned header.
    let mut sticky_cards: Vec<StickyCard> = Vec::new();

    for (mi, msg) in messages.iter().enumerate() {
        // Render blocks
        if msg.is_subagent_task() {
            render_subagent_inline_card(
                frame,
                chat_area,
                msg,
                mi,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
            );
        } else if msg.is_tool_step() {
            render_tool_step_card(
                frame,
                chat_area,
                msg,
                mi,
                selection,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                &mut sticky_cards,
                spinner_phase,
            );
        } else if msg.is_thinking() {
            render_thinking_card(
                frame,
                chat_area,
                msg,
                mi,
                selection,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                &mut sticky_cards,
                spinner_phase,
            );
        } else {
            render_message_blocks(
                frame,
                chat_area,
                msg,
                mi,
                selection,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                true,
            );
        }

        // Spacing between messages. A user message's panel already ends with a
        // bottom transition row (▀) that separates it from the next message, so
        // the extra blank line is omitted there to keep the gap to a single row
        // (otherwise the sent message sits two rows above the following body).
        // The exception is when the next message is a card (thinking or tool
        // step): cards have their own solid background band, and a blank row
        // between the user panel's transition and the card header keeps the two
        // visually distinct. This matches the spacing produced by live reasoning
        // streams and restored history.
        let next_is_card = messages
            .get(mi + 1)
            .is_some_and(|next| next.is_thinking() || next.is_tool_step() || next.is_subagent_task());
        if msg.role != neenee_core::Role::User || next_is_card {
            content_lines += 1;
            if skip_rows > 0 {
                skip_rows = skip_rows.saturating_sub(1);
            } else if current_y < chat_area.y + chat_area.height {
                current_y += 1;
            }
        }
    }

    // Sub-agent navigation band, drawn across the full chat width (inside the
    // app_bg gutters) so it reads as a continuous bar pinned above the input.
    if let (Some(bar), Some(rect)) = (subagent_bar.as_ref(), subagent_bar_rect) {
        draw_subagent_bar(frame, rect, bar, theme);
    }

    // The transient running status lives directly above the input box (a thin
    // animated bar). Hidden while text is actively streaming ("responding"),
    // since the streamed response is itself the feedback in that phase, and
    // hidden when idle.
    let input_rect = if status_active {
        let status_rect = Rect::new(
            chunks[2].x + 2,
            chunks[2].y,
            chunks[2].width.saturating_sub(4),
            1,
        );
        draw_status_bar(frame, status_rect, activity, spinner_phase, theme);
        Rect::new(
            chunks[2].x + 2,
            chunks[2].y + 1,
            chunks[2].width.saturating_sub(4),
            input_box_height,
        )
    } else {
        Rect::new(
            chunks[2].x + 2,
            chunks[2].y,
            chunks[2].width.saturating_sub(4),
            input_box_height,
        )
    };

    // Sticky pinned header: if an expanded card's body covers the top of the
    // viewport (its header is scrolled out of view), pin its header to the line
    // directly under the HUD bar so the user can always collapse it.
    let mut sticky_info = None;
    let first_visible = scroll as usize;
    if let Some(card) = sticky_cards
        .iter()
        .find(|c| c.header_line < first_visible && c.body_end_line > first_visible)
    {
        // Pin inside the same inset band the cards render into so the sticky
        // header aligns exactly with the (possibly scrolled-away) real header.
        let band = chat_band_rect(chat_area);
        let line_rect = Rect::new(band.x, chat_area.y, band.width, 1);
        frame.render_widget(
            Paragraph::new(card_header_line(
                "-",
                &card.header,
                card.color,
                theme.text_muted,
                theme.element_bg,
                band.width as usize,
            )),
            line_rect,
        );
        sticky_info = Some(StickyInfo {
            message_idx: card.message_idx,
            header: card.header.clone(),
            color: card.color,
            block_idx: card.block_idx,
            rect: line_rect,
            header_line: card.header_line,
        });
    }

    ChatRender {
        input_rect,
        content_lines,
        view_height: chat_area.height,
        sticky: sticky_info,
    }
}

fn display_width_u16(s: &str) -> u16 {
    s.width() as u16
}

fn goal_checklist_summary(goal: &Goal) -> Option<(usize, usize, String)> {
    if goal.checklist.is_empty() {
        return None;
    }
    let done = goal
        .checklist
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                neenee_core::GoalChecklistStatus::Completed
                    | neenee_core::GoalChecklistStatus::Cancelled
            )
        })
        .count();
    let current = goal
        .checklist
        .iter()
        .find(|item| item.status == neenee_core::GoalChecklistStatus::InProgress)
        .or_else(|| {
            goal.checklist
                .iter()
                .find(|item| item.status == neenee_core::GoalChecklistStatus::Pending)
        })
        .or_else(|| goal.checklist.last())
        .map(|item| item.content.clone())
        .unwrap_or_default();
    Some((done, goal.checklist.len(), current))
}

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

/// Braille spinner frames used by the transient status bar above the input
/// box. Cycling through these on each frame gives a clear sense of motion
/// (10 frames ≈ one revolution per second at the 100ms tick rate).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame(spinner_phase: usize) -> &'static str {
    SPINNER_FRAMES[spinner_phase % SPINNER_FRAMES.len()]
}

/// Draw the transient activity bar that sits directly above the input box.
/// Replaces the old inline `┃ neenee ⟳ <status>` indicator: the brand prefix
/// is dropped (the header already shows it) and the static `⟳` glyph is
/// replaced by an animated braille spinner so the harness never looks frozen.
pub fn draw_status_bar(
    frame: &mut Frame,
    rect: Rect,
    status: &str,
    spinner_phase: usize,
    theme: &Theme,
) {
    if status.is_empty() || status == "idle" || status == "responding" {
        return;
    }
    let spinner = spinner_frame(spinner_phase);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            spinner,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            status,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::ITALIC),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

/// Draw the right-aligned hint line below the input box. Each entry is
/// `key description`, with the key highlighted and the description muted.
pub fn draw_hint(frame: &mut Frame, rect: Rect, hints: &[(&str, &str)], theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    let entries: Vec<String> = hints
        .iter()
        .map(|(key, desc)| format!("{} {}", key, desc))
        .collect();
    let total: usize = entries.iter().map(|s| s.width()).sum::<usize>() + (entries.len() * 3);
    let width = rect.width as usize;
    let lead = total.min(width);
    if lead < width {
        spans.push(Span::raw(" ".repeat(width - lead)));
    }
    for (i, ((key, _), text)) in hints.iter().zip(entries.iter()).enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(theme.text_muted)));
        }
        spans.push(Span::styled(
            format!("{} ", key),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
        let desc_start = key.width() + 1;
        if desc_start < text.len() {
            spans.push(Span::styled(
                &text[desc_start..],
                Style::default().fg(theme.text_muted),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// Draw a slash-command suggestion popup anchored above the input box.
pub fn draw_suggestions(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    suggestions: &[(&str, &str)],
    selected_idx: Option<usize>,
    anchor: Rect,
    theme: &Theme,
) {
    if suggestions.is_empty() {
        return;
    }

    const MAX_VISIBLE: usize = 6;

    let visible_count = suggestions.len().min(MAX_VISIBLE);
    // +1 line for the bottom hint bar.
    let popup_height = visible_count as u16 + 2;

    // Compute width from content.
    let max_cmd = suggestions
        .iter()
        .map(|(c, _)| c.width())
        .max()
        .unwrap_or(0);
    let max_desc = suggestions
        .iter()
        .map(|(_, d)| d.width())
        .max()
        .unwrap_or(0);
    let inner_width = (max_cmd + 3 + max_desc).max(30) as u16;
    let popup_width = inner_width + 4; // left bar + right padding

    // Position: try above the input box; if not enough room, clamp to top.
    let mut y = anchor.y.saturating_sub(popup_height);
    if y == 0 && anchor.y < popup_height {
        y = 0;
    }
    let viewport = viewport_rect(frame);
    let x = anchor
        .x
        .saturating_add(2)
        .min(viewport.right().saturating_sub(popup_width));

    let area = Rect::new(x, y, popup_width.min(viewport.right() - x), popup_height);
    frame.render_widget(Clear, area);

    let more_hint = if suggestions.len() > MAX_VISIBLE {
        format!(" … +{} more ", suggestions.len() - MAX_VISIBLE)
    } else {
        " ".to_string()
    };

    let block = RtBlock::default()
        .borders(Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(theme.primary))
        .style(Style::default().bg(theme.menu_bg));

    // Custom list: render the block, then items inside, plus a title line.
    let title = Line::from(vec![Span::styled(
        " Commands",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )]);

    let mut lines: Vec<Line> = vec![title];
    lines.extend(
        suggestions
            .iter()
            .take(MAX_VISIBLE)
            .enumerate()
            .map(|(i, (cmd, desc))| {
                let is_selected = Some(i) == selected_idx;
                let style = if is_selected {
                    Style::default()
                        .bg(theme.primary)
                        .fg(contrast_fg(theme.primary))
                } else {
                    Style::default().fg(theme.text)
                };
                let cmd_style = if is_selected {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
                };
                Line::from(vec![
                    Span::styled(" ", Style::default()),
                    Span::styled(format!("{:<width$} ", cmd, width = max_cmd), cmd_style),
                    Span::styled("· ", Style::default().fg(theme.text_muted)),
                    Span::styled(
                        *desc,
                        if is_selected {
                            style
                        } else {
                            Style::default().fg(theme.text_muted)
                        },
                    ),
                ])
            }),
    );
    lines.push(Line::from(Span::styled(
        format!("{}↑↓ navigate · Enter select · Esc close", more_hint),
        Style::default().fg(theme.text_muted),
    )));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the models modal. `key_status` maps lowercase provider names to
/// whether a usable API key is available (env or config).
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    solutions: &[crate::ModelSolution],
    current_provider: &str,
    modal_index: usize,
    key_status: &HashMap<String, bool>,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(72, 60, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(vec![Span::styled(
        " Select Model Solution",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )])];

    for (i, solution) in solutions.iter().enumerate() {
        let is_current = solution.id == current_provider;
        let is_selected = i == modal_index;
        let row_bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let row_fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let base = Style::default().bg(row_bg).fg(row_fg);
        let (key_label, key_color) = match key_status.get(solution.id) {
            Some(true) => ("✓ ready", theme.success),
            Some(false) => ("✗ no key", theme.error_fg),
            None => ("", row_fg),
        };
        let key_style = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(key_color)
        };
        let prefix = if is_current { "● " } else { "  " };
        let dim = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(theme.text_muted)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", prefix), dim),
            Span::styled(
                format!("{:<14} ", solution.name),
                base.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:<9} ", key_label), key_style),
            Span::styled(format!("{} ", solution.model), dim),
            Span::styled(format!("· {}", solution.description), dim),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter select/setup · k configure · Esc ",
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn draw_solution_input_modal(
    frame: &mut Frame,
    title: &str,
    help: &str,
    value: &str,
    masked: bool,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(60, 30, viewport_rect(frame));
    frame.render_widget(Clear, area);
    let display = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", title),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" {}", help),
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.primary)),
            Span::styled(display, Style::default().fg(theme.text)),
            Span::styled("▏", Style::default().fg(theme.primary)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Enter continue · Esc cancel ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the API-key entry modal. The key itself is already masked by the caller.
pub fn draw_api_key_modal(frame: &mut Frame, provider: &str, masked_key: &str, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(56, 34, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" API key · {}", provider),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Key  ", Style::default().fg(theme.text_muted)),
            Span::styled(
                masked_key.to_string(),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(theme.primary)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Saved to the local config file; environment",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(Span::styled(
            " variables of the same provider still win.",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Enter save & switch · Esc cancel ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render a unix timestamp as a short relative time (e.g. "2h ago", "3d ago").
pub fn relative_time(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(ts);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 7 * 86_400 {
        format!("{}d ago", diff / 86_400)
    } else {
        format!("{}w ago", diff / (7 * 86_400))
    }
}

/// Draw the sessions picker: each row shows the session overview plus its
/// creation and last-interaction times. Enter opens the selected session.
pub fn draw_sessions_modal(
    frame: &mut Frame,
    sessions: &[neenee_core::SessionOverview],
    selected: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(80, 64, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Sessions",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))];

    if sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " No previous sessions yet.",
            Style::default().fg(theme.text_muted),
        )));
    }

    for (i, session) in sessions.iter().enumerate() {
        let is_selected = i == selected;
        let bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let muted = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text_muted
        };
        let badge = if session.active { "● " } else { "  " };
        let overview: String = session.overview.chars().take(48).collect();
        let meta = format!(
            "{} msgs · created {} · active {}",
            session.message_count,
            relative_time(session.created_at),
            relative_time(session.updated_at)
        );
        let overview_used = 1 + badge.len() + overview.width();
        let meta_used = 2 + meta.width();
        // Right-align the meta on the same row when it fits.
        let inner_width = area.width.saturating_sub(2) as usize;
        let gap = inner_width.saturating_sub(overview_used.min(inner_width / 2) + meta_used);
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}{}", badge, overview),
                Style::default().bg(bg).fg(fg),
            ),
            Span::styled(" ".repeat(gap), Style::default().bg(bg)),
            Span::styled(format!("  {}  ", meta), Style::default().bg(bg).fg(muted)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter open · d delete · Esc close ",
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the history search modal.
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    modal_index: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(70, 55, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Chat History",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))];

    for (i, h) in history.iter().enumerate() {
        let is_selected = i == modal_index;
        let bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let num_style = if is_selected {
            Style::default().bg(bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(theme.text_muted)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>3} ", i + 1), num_style),
            Span::styled(h, Style::default().bg(bg).fg(fg)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter insert · Esc close ",
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a blocking tool permission request as a bottom-anchored sheet
/// (opencode-style): dimmed backdrop above, a panel with a warning-colored
/// left bar, the tool/scope/arguments detail, and a footer bar of inline
/// options where the selected one is highlighted.
pub fn draw_permission_sheet(
    frame: &mut Frame,
    request: &PermissionRequest,
    selected: usize,
    confirm_always: bool,
    scroll: usize,
    theme: &Theme,
) -> usize {
    let size = viewport_rect(frame);
    let bottom = size.height;

    let arguments = serde_json::from_str::<serde_json::Value>(&request.arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| request.arguments.clone());
    let arg_lines: Vec<Line> = arguments.lines().map(Line::from).collect();

    let labels: &[&str] = if confirm_always {
        &["Confirm always", "Cancel"]
    } else {
        &["Allow once", "Always allow", "Reject"]
    };

    let mut body_lines: Vec<Line> = Vec::new();
    body_lines.push(Line::from(vec![
        Span::styled("△ ", Style::default().fg(theme.warning)),
        Span::styled(
            "Permission required",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(vec![
        Span::styled("Tool ", Style::default().fg(theme.text_muted)),
        Span::styled(
            request.tool.clone(),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Scope ", Style::default().fg(theme.text_muted)),
        Span::styled(request.scope.clone(), Style::default().fg(theme.info)),
    ]));
    body_lines.push(Line::from(Span::styled(
        request.description.clone(),
        Style::default().fg(theme.text),
    )));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(Span::styled(
        "Arguments",
        Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
    )));
    body_lines.extend(
        arg_lines
            .into_iter()
            .map(|line| line.style(Style::default().fg(theme.code_fg))),
    );
    if confirm_always {
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            "This permits the tool until neenee exits.",
            Style::default().fg(theme.warning),
        )));
    }

    let available_width = size.width.saturating_sub(2 * CHAT_H_INSET).max(1);
    let preferred_width = available_width.saturating_mul(PERMISSION_SHEET_WIDTH_PERCENT) / 100;
    let min_width = PERMISSION_SHEET_MIN_WIDTH.min(available_width);
    let sheet_width = preferred_width
        .max(min_width)
        .min(PERMISSION_SHEET_MAX_WIDTH)
        .min(available_width)
        .max(1);
    let sheet_x = size.x + size.width.saturating_sub(sheet_width) / 2;

    let input_gap = PERMISSION_SHEET_INPUT_GAP.min(bottom);
    let sheet_bottom = bottom.saturating_sub(input_gap);
    let footer_height: u16 = 1;
    let footer_gap: u16 = 1;
    let fixed_height =
        PERMISSION_SHEET_TOP_PADDING + footer_gap + footer_height + PERMISSION_SHEET_BOTTOM_PADDING;
    let max_h = PERMISSION_SHEET_MAX_HEIGHT.min(sheet_bottom).max(1);
    let content_w = sheet_width
        .saturating_sub(1 + 2 * PERMISSION_SHEET_H_PADDING)
        .max(1);
    let body_total_rows: usize = body_lines
        .iter()
        .map(|line| {
            let width: usize = line.spans.iter().map(|span| span.content.width()).sum();
            width.max(1).div_ceil(content_w as usize)
        })
        .sum();
    let body_capacity = max_h.saturating_sub(fixed_height).max(1);
    let body_h = (body_total_rows as u16).min(body_capacity);
    let max_scroll = body_total_rows.saturating_sub(body_h as usize);
    let body_scroll = scroll.min(max_scroll);
    let sheet_h = (fixed_height + body_h).min(sheet_bottom).max(1);
    let sheet_top = sheet_bottom.saturating_sub(sheet_h);

    draw_dim_backdrop(frame, size, theme.backdrop);

    let area = Rect::new(sheet_x, size.y + sheet_top, sheet_width, sheet_h);
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(theme.warning, theme.panel_bg), area);

    let content_x = area.x + 1 + PERMISSION_SHEET_H_PADDING;
    let body_area = Rect::new(
        content_x,
        area.y + PERMISSION_SHEET_TOP_PADDING,
        content_w,
        body_h,
    );
    frame.render_widget(
        Paragraph::new(body_lines)
            .scroll((body_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        body_area,
    );

    let footer_y = area
        .y
        .saturating_add(sheet_h)
        .saturating_sub(PERMISSION_SHEET_BOTTOM_PADDING + footer_height);
    let footer_band = Rect::new(area.x + 1, footer_y, area.width.saturating_sub(1), 1);
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.element_bg)),
        footer_band,
    );

    let mut footer_spans: Vec<Span> = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let is_reject = !confirm_always && index == 2;
        let is_selected = index == selected;
        let bg = if is_selected {
            if is_reject {
                theme.error_fg
            } else {
                theme.warning
            }
        } else {
            theme.element_bg
        };
        let fg = if is_selected {
            contrast_fg(bg)
        } else {
            theme.text
        };
        if index > 0 {
            footer_spans.push(Span::styled("  ", Style::default().bg(theme.element_bg)));
        }
        footer_spans.push(Span::styled(
            format!(" {} ", label),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
    }
    let hint = if max_scroll > 0 {
        " ↑↓/PgUp/PgDn scroll · ←→ select · Enter confirm · Esc reject "
    } else {
        " ←→ select · Enter confirm · Esc reject "
    };
    let hint_width = hint.width();
    let footer_width = content_w as usize;
    let used: usize = footer_spans.iter().map(|s| s.content.width()).sum();
    if used + hint_width <= footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used - hint_width),
            Style::default().bg(theme.element_bg),
        ));
        footer_spans.push(Span::styled(
            hint,
            Style::default().bg(theme.element_bg).fg(theme.text_muted),
        ));
    } else if used < footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used),
            Style::default().bg(theme.element_bg),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(footer_spans)),
        Rect::new(content_x, footer_y, content_w, 1),
    );
    max_scroll
}

/// Draw an armed-action toast (e.g. "press Ctrl+C again to exit",
/// "press Esc again to interrupt"). Warn-colored like the original exit toast.
pub fn draw_armed_toast(frame: &mut Frame, message: &str, theme: &Theme) {
    let size = frame.size();
    toast(frame, theme, message, theme.warning, size.width);
}

/// Draw the help / keybindings modal.
pub fn draw_help_modal(frame: &mut Frame, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(58, 70, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let key = |k: &str| {
        Span::styled(
            format!("{:<10}", k),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &str| Span::styled(d.to_string(), Style::default().fg(theme.text_muted));
    let section = |title: &str| {
        Span::styled(
            title.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )
    };
    let row = |k: &str, d: &str| Line::from(vec![key(k), desc(d)]);

    let lines = vec![
        Line::from(Span::styled(
            " Help",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("alt+enter", "insert newline (ctrl+j)"),
        row("esc", "interrupt (×2) / close"),
        row("ctrl+c", "copy · interrupt · quit (×2)"),
        row("↑ / ↓", "history · navigate"),
        row("tab", "accept suggestion"),
        Line::from(""),
        Line::from(section("Views & tools")),
        row("ctrl+h", "this help"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("/", "slash commands"),
        Line::from(""),
        Line::from(section("Modes")),
        row("/mode", "build · plan"),
        row("/goal", "set or manage the goal"),
        row("/loop N", "bounded autonomous work"),
        Line::from(""),
        Line::from(desc("Drag to select · Ctrl+C or Ctrl+Shift+C to copy.")),
        Line::from(""),
        Line::from(Span::styled(
            " esc · close ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a "copied to clipboard" toast.
pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.size();
    let color = if failed {
        theme.error_fg
    } else {
        theme.success
    };
    toast(frame, theme, message, color, size.width);
}

/// opencode-style toast: top-right panel with variant-colored left/right bars.
fn toast(frame: &mut Frame, theme: &Theme, message: &str, color: Color, width: u16) {
    let text = format!(" {} ", message.trim());
    // Inner width (text) capped, plus the two border columns.
    let inner_w = text.width() as u16;
    let toast_width = inner_w.min(58) + 2;
    let x = width.saturating_sub(toast_width).saturating_sub(2).max(1);
    let area = Rect::new(x, 1, toast_width, 3);

    let block = RtBlock::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel_bg));

    let line = Line::from(Span::styled(
        text,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ));
    // Vertically center the single line within the 3-row panel.
    let para = Paragraph::new(vec![Line::from(""), line]);
    frame.render_widget(para.block(block), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Fake an alpha backdrop by filling an area with a dim solid color.
fn draw_dim_backdrop(frame: &mut Frame, area: Rect, color: Color) {
    frame.render_widget(RtBlock::default().style(Style::default().bg(color)), area);
}

/// A borderless panel with a single thick colored left bar (opencode-style).
fn panel_block(bar_color: Color, bg: Color) -> RtBlock<'static> {
    RtBlock::default()
        .borders(Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(bar_color))
        .style(Style::default().bg(bg))
}

/// Contrast foreground for a colored background (dark text on light fills).
fn contrast_fg(bg: Color) -> Color {
    let (r, g, b) = rgb(bg);
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance > 140.0 {
        Color::Black
    } else {
        Color::White
    }
}

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (224, 108, 117),
        Color::Green => (127, 216, 143),
        Color::Yellow => (229, 192, 123),
        Color::Blue => (137, 180, 250),
        Color::Magenta => (203, 166, 247),
        Color::Cyan => (86, 182, 194),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightGreen => (127, 216, 143),
        Color::LightRed => (243, 139, 168),
        _ => (128, 128, 128),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-render every redesigned component into a buffer to catch panics
    /// (border math, rect underflows, empty content) without a live terminal.
    #[test]
    fn redesigned_components_render_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let mut thinking = ChatMessage::thinking("Reasoning about the task step by step.");
                thinking.set_thinking_expanded(true);
                let mut tool = ChatMessage::tool_step("call_1", "list_dir", r#"{"path":"."}"#);
                tool.set_tool_step_expanded(true);
                tool.finish_tool_step("call_1", "file_a\nfile_b", 12);
                let messages = vec![
                    ChatMessage::new(neenee_core::Role::User, "hi"),
                    ChatMessage::new(
                        neenee_core::Role::Assistant,
                        "Here is a table:\n\n| Tool | Count |\n| --- | ---: |\n| read | 1 |\n| webfetch | 250 |",
                    ),
                    thinking,
                    tool,
                ];
                let _ = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "waiting for model",
                        spinner_phase: 0,
                        input: "hello",
                        chrome_hidden: false,
                        subagent_bar: None,
                        theme: &theme,
                    },
                );
                draw_input(
                    f,
                    Rect::new(0, 21, 80, 3),
                    "hello",
                    5,
                    theme.accent,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                );
                draw_hint(
                    f,
                    Rect::new(0, 24, 80, 1),
                    &[("ctrl+p", "commands"), ("ctrl+h", "help"), ("enter", "send")],
                    &theme,
                );
                draw_suggestions(
                    f,
                    &mut layout_map,
                    &[("/goal", "Set goal"), ("/clear", "Clear")],
                    Some(0),
                    Rect::new(0, 20, 80, 3),
                    &theme,
                );
                draw_copy_toast(f, "copied to clipboard", false, &theme);
                draw_armed_toast(f, "press Ctrl+C again to exit", &theme);
            })
            .unwrap();

        // Modals + permission sheet on a fresh frame.
        terminal
            .draw(|f| {
                draw_models_modal(
                    f,
                    &mut LayoutMap::new(),
                    &[],
                    "mock",
                    0,
                    &HashMap::new(),
                    &theme,
                );
                draw_history_modal(f, &mut LayoutMap::new(), &["a".to_string()], 0, &theme);
                draw_api_key_modal(f, "openai", "sk-•••", &theme);
                draw_solution_input_modal(f, " Endpoint", "url", "https://x", false, &theme);
                draw_help_modal(f, &theme);
                draw_sessions_modal(
                    f,
                    &[
                        neenee_core::SessionOverview {
                            id: "abc123".to_string(),
                            overview: "Refactor the renderer".to_string(),
                            created_at: 0,
                            updated_at: 0,
                            message_count: 12,
                            active: true,
                        },
                        neenee_core::SessionOverview {
                            id: "def456".to_string(),
                            overview: "Fix the tool_call_id bug".to_string(),
                            created_at: 0,
                            updated_at: 0,
                            message_count: 4,
                            active: false,
                        },
                    ],
                    0,
                    &theme,
                );
            })
            .unwrap();

        terminal
            .draw(|f| {
                let request = PermissionRequest {
                    id: "p1".to_string(),
                    tool: "bash".to_string(),
                    description: "run a command".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                    scope: "*".to_string(),
                };
                let _ = draw_permission_sheet(f, &request, 0, false, 0, &theme);
            })
            .unwrap();
    }

    /// Render both the compact sub-agent card (root view) and the zoomed-in
    /// sub-agent view with its navigation bar, ensuring no layout panics.
    #[test]
    fn subagent_card_and_view_render_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Root view: a completed sub-agent task renders as a compact card.
        let mut task = ChatMessage::tool_step(
            "task_1",
            "task",
            r#"{"description":"explore the codebase","prompt":"..."}"#,
        );
        task.push_subtask_event(&neenee_core::SubTaskEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        task.finish_tool_step("task_1", "found 3 matches", 1200);
        let root_messages = vec![
            ChatMessage::new(neenee_core::Role::User, "explore please"),
            task,
        ];

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &root_messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "running subagent",
                        spinner_phase: 0,
                        input: "",
                        chrome_hidden: false,
                        subagent_bar: None,
                        theme: &theme,
                    },
                );
            })
            .unwrap();

        // Zoomed-in sub-agent view: the task's children are the message stream
        // and the navigation bar is shown.
        let children = root_messages[1].subagent_children().unwrap().to_vec();
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &children,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "",
                        spinner_phase: 0,
                        input: "",
                        chrome_hidden: false,
                        subagent_bar: Some(SubagentBarInfo {
                            label: "explore the codebase".to_string(),
                            index: 1,
                            total: 1,
                        }),
                        theme: &theme,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn line_selection_intersects_wrapped_lines() {
        use crate::layout::SemanticCursor;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 2),
            head: SemanticCursor::new(0, 0, 8),
        };
        let range = block_selection_range(&sel, 0, 0);

        // Line covering bytes 0..5 ("hello"): selected from 2 to end.
        let first = WrappedLine {
            text: "hello".to_string(),
            start_byte: 0,
            end_byte: 5,
        };
        assert_eq!(line_selection(range, &first), Some((2, 5)));

        // Line covering bytes 5..10 ("world"): selected up to head char (8 → rel 3, inclusive → 4).
        let second = WrappedLine {
            text: "world".to_string(),
            start_byte: 5,
            end_byte: 10,
        };
        assert_eq!(line_selection(range, &second), Some((0, 4)));

        // A line after the selection has no overlap.
        let third = WrappedLine {
            text: "after".to_string(),
            start_byte: 10,
            end_byte: 15,
        };
        assert_eq!(line_selection(range, &third), None);
    }

    #[test]
    fn block_selection_covers_middle_blocks_fully() {
        use crate::layout::SemanticCursor;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 3),
            head: SemanticCursor::new(0, 2, 1),
        };
        assert_eq!(block_selection_range(&sel, 0, 0), Some((3, None)));
        assert_eq!(block_selection_range(&sel, 0, 1), Some((0, None)));
        assert_eq!(block_selection_range(&sel, 0, 2), Some((0, Some(1))));
        assert_eq!(block_selection_range(&sel, 0, 3), None);
        assert_eq!(block_selection_range(&sel, 1, 0), None);
    }

    #[test]
    fn test_wrap_text() {
        let lines = wrap_text("hello world", 5);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text, " worl");
        assert_eq!(lines[2].text, "d");
    }

    #[test]
    fn test_wrap_with_newlines() {
        let lines = wrap_text("hi\nthere", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "hi");
        assert_eq!(lines[1].text, "there");
    }

    #[test]
    fn wrap_avoids_cjk_punctuation_at_line_start() {
        let lines = wrap_text("人生需要坚持，才能前进。", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().skip(1).all(|line| line
            .text
            .chars()
            .next()
            .is_none_or(|ch| !prohibited_line_start(ch))));
        assert!(lines.iter().all(|line| line
            .text
            .chars()
            .last()
            .is_none_or(|ch| !prohibited_line_end(ch))));
    }

    #[test]
    fn checklist_summary_prefers_current_work() {
        let goal = Goal {
            objective: "ship".to_string(),
            status: GoalStatus::Active,
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
            checklist: vec![
                neenee_core::GoalChecklistItem {
                    content: "implemented".to_string(),
                    status: neenee_core::GoalChecklistStatus::Completed,
                },
                neenee_core::GoalChecklistItem {
                    content: "run tests".to_string(),
                    status: neenee_core::GoalChecklistStatus::InProgress,
                },
            ],
        };

        assert_eq!(
            goal_checklist_summary(&goal),
            Some((1, 2, "run tests".to_string()))
        );
    }

    /// The input box must reserve only a single content row for a short input
    /// but grow to fit wrapped text when the input is long.
    #[test]
    fn input_box_grows_with_wrapped_content() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let messages: Vec<ChatMessage> = Vec::new();

        fn render_with(theme: &Theme, messages: &[ChatMessage], input: &str) -> Rect {
            let backend = TestBackend::new(40, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut rect = Rect::default();
            terminal
                .draw(|f| {
                    let mut layout_map = LayoutMap::new();
                    let r = draw_chat(
                        f,
                        &mut layout_map,
                        ChatView {
                            messages,
                            scroll: 0,
                            selection: &SelectionState::None,
                            current_provider: "mock",
                            current_model: "m",
                            current_mode: AgentMode::Build,
                            current_goal: None,
                            activity: "",
                            spinner_phase: 0,
                            input,
                            chrome_hidden: false,
                            subagent_bar: None,
                            theme,
                        },
                    );
                    rect = r.input_rect;
                })
                .unwrap();
            rect
        }

        // Short input: one content line + two padding rows = 3.
        let short = render_with(&theme, &messages, "hi");
        assert_eq!(short.height, 3);

        // Long input wraps across many lines on a 40-wide terminal; the box
        // must grow beyond the single-line baseline.
        let long_input = "word ".repeat(40);
        let tall = render_with(&theme, &messages, &long_input);
        assert!(
            tall.height > 3,
            "wrapped input should grow the box, got height {}",
            tall.height
        );
        // ...but never more than half the terminal.
        assert!(tall.height <= 12);
    }

    /// `draw_input` must not panic for tricky inputs and should place the caret
    /// on the second wrapped line when the cursor sits past the first wrap.
    #[test]
    fn draw_input_wraps_and_positions_caret() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        // "aaaa bbbb cccc" wraps within the ~17-wide inner area; cursor at the
        // very end should be on a later line, not off the box.
        let input = "aaaa bbbb cccc dddd eeee";
        terminal
            .draw(|f| {
                draw_input(
                    f,
                    Rect::new(0, 0, 20, 8),
                    input,
                    input.width() as u16,
                    theme.accent,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                );
            })
            .unwrap();
    }

    /// Wide tables (including CJK content) must keep borders intact and never
    /// overflow the viewport: columns shrink to fit, cell text wraps, and
    /// every rendered line stays within the available width.
    #[test]
    fn wide_table_shrinks_columns_and_keeps_borders_intact() {
        use crate::document::TableAlignment;

        let headers = vec![
            "工具".to_string(),
            "类型".to_string(),
            "底层实现".to_string(),
            "关键特性".to_string(),
        ];
        let rows = vec![
            vec![
                "bash".to_string(),
                "Write".to_string(),
                "std::process::Command（sh -c / cmd /C）".to_string(),
                "执行 shell 命令，支持 timeout，输出截断".to_string(),
            ],
            vec![
                "read_file".to_string(),
                "ReadOnly".to_string(),
                "std::fs::read_to_string".to_string(),
                "支持 offset/limit".to_string(),
            ],
        ];
        let aligns = vec![
            TableAlignment::None,
            TableAlignment::None,
            TableAlignment::None,
            TableAlignment::None,
        ];

        // ── Narrow terminal (34 cols): table is far wider, must shrink ──
        let lines = build_table_render(&headers, &rows, &aligns, 34).lines;
        assert!(!lines.is_empty(), "table must produce output");

        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.width() <= 34,
                "line {i} overflows: {} cols: {}",
                line.width(),
                line
            );
        }
        assert!(lines.first().unwrap().starts_with('┌'));
        assert!(lines.last().unwrap().starts_with('└'));
        assert!(
            lines.iter().any(|l| l.starts_with('├')),
            "missing header/body separator"
        );
        // Two body rows → one separator between them (plus one after header).
        let sep_count = lines.iter().filter(|l| l.starts_with('├')).count();
        assert_eq!(
            sep_count, 2,
            "expected 2 separators (header→body + row→row), got {sep_count}"
        );
        let pipe_counts: Vec<usize> = lines
            .iter()
            .filter(|l| l.starts_with('│'))
            .map(|l| l.matches('│').count())
            .collect();
        assert!(!pipe_counts.is_empty(), "must have data lines");
        assert!(
            pipe_counts.iter().all(|&c| c == pipe_counts[0]),
            "all data lines must have the same number of column separators"
        );

        // ── Wide terminal (80 cols): table fits without shrinking ──
        let wide_lines = build_table_render(&headers, &rows, &aligns, 76).lines;
        for (i, line) in wide_lines.iter().enumerate() {
            assert!(
                line.width() <= 76,
                "wide line {i} overflows: {} cols",
                line.width()
            );
        }
        // When it fits, the table should be shorter (no wrapping needed).
        assert!(
            wide_lines.len() <= lines.len(),
            "wide table should have fewer lines than shrunk table"
        );
    }

    #[test]
    fn shrink_columns_preserves_minimum_and_proportions() {
        // Intrinsic [10, 5, 20], target 24, min 3.
        // total_min = 9, shrinkable = 26, available = 15.
        // col0: 3 + 7*15/26 = 3 + 4 = 7
        // col1: 3 + 2*15/26 = 3 + 1 = 4
        // col2: 3 + 17*15/26 = 3 + 9 = 12
        let result = shrink_column_widths(&[10, 5, 20], 24, 3);
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|&w| w >= 3), "must respect minimum");
        assert!(
            result.iter().sum::<usize>() <= 24,
            "must fit within target, got {}",
            result.iter().sum::<usize>()
        );
        // Largest intrinsic column stays largest after shrinking.
        let max_val = *result.iter().max().unwrap();
        let max_idx = result.iter().position(|&v| v == max_val).unwrap();
        assert_eq!(max_idx, 2);
    }

    #[test]
    fn shrink_columns_with_tiny_target_returns_all_minimum() {
        let result = shrink_column_widths(&[10, 20, 30], 5, 3);
        assert_eq!(result, vec![3, 3, 3]);
    }
}
