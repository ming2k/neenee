//! Expandable card renderers: tool-step, thinking, child tool step, sub-agent
//! task, and bash preview, plus their per-tool content renderers (code,
//! listing, grep, bash) and shared header / section helpers. Also produces
//! the sticky pinned-card header that [`super::draw_transcript`] overlays while a
//! card body is scrolled into view.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::document::{Block, TranscriptMessage};
use crate::layout::{
    BlockRegion, InteractiveTarget, LayoutMap, THINKING_BLOCK_IDX, TOOL_STEP_BLOCK_IDX,
};
use crate::selection::SelectionState;

use super::chrome::spinner_frame;
use super::message_body::draw_message_body;
use super::text_layout::{
    block_selection_range, code_gutter_line, line_selection, line_spans, padded_tail, wrap_text,
    WrappedLine,
};
use super::tools::{ArgLayout, DiffLine, DiffOp, PreviewLine, PreviewTone, ResultKind, ToolStatus};
use super::{
    transcript_band_rect, StickyInfo, SubagentBarInfo, Theme, CARD_MIN_WIDTH,
    REASONING_TRACE_BLOCK_GAP_ROWS, REASONING_TRACE_BODY_BOTTOM_GAP_ROWS,
    REASONING_TRACE_BODY_TOP_GAP_ROWS, TOOL_CARD_BODY_BOTTOM_GAP_ROWS, TOOL_CARD_BODY_TOP_GAP_ROWS,
    TOOL_CARD_CHILDREN_GAP_ROWS, TOOL_CARD_SECTION_GAP_ROWS, TRANSCRIPT_BODY_PREFIX_COLS,
    TRANSCRIPT_BODY_RIGHT_INSET, TRANSCRIPT_H_INSET,
};

/// Tracked info for an expanded card, used to render a sticky header pinned
/// under the HUD bar while the card's body is scrolled into view.
pub(super) struct StickyCard {
    message_idx: usize,
    header: String,
    color: Color,
    /// Tool glyph shown in the pinned header so it matches the real header
    /// (a space for reasoning traces, which have no tool icon).
    icon: char,
    background: Option<Color>,
    /// usize::MAX for tool steps, usize::MAX - 1 for reasoning traces.
    block_idx: usize,
    header_line: usize,
    body_end_line: usize,
}

/// Build the header band of an expandable card: a solid background region
/// (no border lines) reading `+ {header}` when collapsed or `- {header}` when
/// expanded, padded to the full width so it reads as a colored band. The body
/// content is expected to start at column 2 so it left-aligns with the header
/// text.
/// Build a tool-card header band: an optional expand marker (`+`/`-`), an
/// optional status glyph (the colored "rail" — spinner / `✓` / `✗`), the tool
/// icon, and the summary, padded to a full-width colored band.
///
/// Only the status glyph carries `status_color`; the expand marker, icon, and
/// summary use the muted `header_color`, so color reads purely as run state.
/// Empty `expand` / `status_glyph` segments (and their trailing space) are
/// skipped so the subagent and pinned headers can omit them cleanly.
fn tool_header_line(
    expand: &str,
    status_glyph: &str,
    status_color: Color,
    icon: char,
    header: &str,
    header_color: Color,
    bg: Color,
    full_width: usize,
) -> Line<'static> {
    let base = Style::default().bg(bg);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    let mut used = 0usize;

    if !expand.is_empty() {
        let s = format!("{} ", expand);
        used += s.width();
        spans.push(Span::styled(
            s,
            base.fg(header_color).add_modifier(Modifier::BOLD),
        ));
    }
    if !status_glyph.is_empty() {
        let s = format!("{} ", status_glyph);
        used += s.width();
        spans.push(Span::styled(
            s,
            base.fg(status_color).add_modifier(Modifier::BOLD),
        ));
    }
    let icon_s = format!("{} ", icon);
    used += icon_s.width();
    spans.push(Span::styled(icon_s, base.fg(header_color)));

    used += header.width();
    spans.push(Span::styled(
        header.to_string(),
        base.fg(header_color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(padded_tail(full_width, used), base));
    Line::from(spans)
}

/// Render the shared header band of an expandable card and record its rect in
/// the layout map so clicks / `Enter` on it can toggle the card. Returns the
/// content-line index of the header (used for sticky-pin tracking).
///
/// `block_idx` is the sentinel recorded in [`BlockRegion`] so the click handler
/// can tell card/trace kinds apart: `usize::MAX` for tool-step cards and
/// `usize::MAX - 1` for reasoning traces.
#[allow(clippy::too_many_arguments)]
fn draw_expandable_card_header(
    frame: &mut Frame,
    transcript_area: Rect,
    full_width: usize,
    mi: usize,
    block_idx: usize,
    expanded: bool,
    status_glyph: &str,
    status_color: Color,
    icon: char,
    header: &str,
    header_color: Color,
    bg: Color,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) -> usize {
    let expand = if expanded { "-" } else { "+" };
    let header_line_idx = *content_lines;

    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < transcript_area.y + transcript_area.height {
        let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
        frame.render_widget(
            Paragraph::new(tool_header_line(
                expand,
                status_glyph,
                status_color,
                icon,
                header,
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

/// Draw blank rows padded to `full_width` with `style`'s background. The row
/// count is supplied by component spacing tokens in `design.rs`.
fn draw_blank_rows(
    frame: &mut Frame,
    transcript_area: Rect,
    full_width: usize,
    style: Style,
    rows: usize,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    for _ in 0..rows {
        *content_lines += 1;
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
        } else if *current_y < transcript_area.y + transcript_area.height {
            let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(padded_tail(full_width, 0), style))),
                rect,
            );
            *current_y += 1;
        }
    }
}

/// Draw a section label line (" Tool", " Arguments", " Result") inside an
/// expanded card body. The label sits at column 1 (one-space prefix) in
/// `label_style`; the rest of the band is filled with `pad_style`'s
/// background so it reads as a solid colored row.
#[allow(clippy::too_many_arguments)]
fn draw_section_label(
    frame: &mut Frame,
    transcript_area: Rect,
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
    } else if *current_y < transcript_area.y + transcript_area.height {
        let indent = 2usize;
        let used = indent + label.len();
        let line = Line::from(vec![
            Span::styled(" ".repeat(indent), pad_style),
            Span::styled(label, label_style),
            Span::styled(padded_tail(full_width, used), pad_style),
        ]);
        let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }
}

/// Render a shell command line inside an expanded bash tool card. Long
/// commands wrap under the prompt so the expanded view stays compact without
/// losing the actual command that ran.
#[allow(clippy::too_many_arguments)]
fn draw_meta_value_row_wrapped(
    frame: &mut Frame,
    transcript_area: Rect,
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
        if *current_y >= transcript_area.y + transcript_area.height {
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
        let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }
}

/// Render one labelled section (Arguments / a plain Result fallback) inside
/// an expanded tool-step card body. Handles scroll-skip, wrapping, semantic
/// selection layout recording, and an optional blank separator above the
/// label. Result rendering for known tools is handled by
/// [`draw_tool_result_section`] instead.
#[allow(clippy::too_many_arguments)]
fn draw_tool_body_section(
    frame: &mut Frame,
    transcript_area: Rect,
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
        draw_blank_rows(
            frame,
            transcript_area,
            full_width,
            pad_style,
            TOOL_CARD_SECTION_GAP_ROWS,
            skip_rows,
            current_y,
            content_lines,
        );
    }

    draw_section_label(
        frame,
        transcript_area,
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
        draw_code_content(
            frame,
            transcript_area,
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
            if *current_y >= transcript_area.y + transcript_area.height {
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

            let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
fn draw_code_content(
    frame: &mut Frame,
    transcript_area: Rect,
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
            if *current_y >= transcript_area.y + transcript_area.height {
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
            let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
fn draw_listing_content(
    frame: &mut Frame,
    transcript_area: Rect,
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
            if *current_y >= transcript_area.y + transcript_area.height {
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
            let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
    transcript_area: Rect,
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
        if *current_y >= transcript_area.y + transcript_area.height {
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
        let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
fn draw_grep_content(
    frame: &mut Frame,
    transcript_area: Rect,
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
                        transcript_area,
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
                    if *current_y >= transcript_area.y + transcript_area.height {
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
                    let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
                    transcript_area,
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
fn draw_bash_content(
    frame: &mut Frame,
    transcript_area: Rect,
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
            if *current_y >= transcript_area.y + transcript_area.height {
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
            let rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
/// line-numbered code block via [`draw_code_content`].
#[allow(clippy::too_many_arguments)]
fn draw_tool_result_section(
    frame: &mut Frame,
    transcript_area: Rect,
    full_width: usize,
    mi: usize,
    name: &str,
    arguments: &str,
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
        draw_blank_rows(
            frame,
            transcript_area,
            full_width,
            body_pad,
            TOOL_CARD_SECTION_GAP_ROWS,
            skip_rows,
            current_y,
            content_lines,
        );
    }
    let kind = super::tools::presenter_for(name).result_kind();
    // Diff steps relabel the section so it reads as a change, not raw output.
    let label = if kind == ResultKind::Diff {
        "Diff"
    } else {
        "Result"
    };
    draw_section_label(
        frame,
        transcript_area,
        full_width,
        label,
        body_pad,
        body_label,
        skip_rows,
        current_y,
        content_lines,
    );

    let block_idx = 1usize;
    match kind {
        ResultKind::Listing => draw_listing_content(
            frame,
            transcript_area,
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
        ResultKind::Grep => draw_grep_content(
            frame,
            transcript_area,
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
        ResultKind::Bash => draw_bash_content(
            frame,
            transcript_area,
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
        ResultKind::Code => draw_code_content(
            frame,
            transcript_area,
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
        ResultKind::Diff => draw_diff_content(
            frame,
            transcript_area,
            full_width,
            &super::tools::diff_lines_for(name, arguments),
            theme,
            skip_rows,
            current_y,
            content_lines,
            indent,
            inner_w,
        ),
    }
}

/// Render a red/green line diff inside an expanded edit/write card body. Each
/// [`DiffLine`] is a row in the `code_bg` block: a colored `+`/`-`/` ` sign
/// gutter then the (wrapped) line text. The diff is a derived view of the
/// tool's arguments, so rows aren't registered for text selection.
#[allow(clippy::too_many_arguments)]
fn draw_diff_content(
    frame: &mut Frame,
    transcript_area: Rect,
    full_width: usize,
    diff: &[DiffLine],
    theme: &Theme,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    indent: usize,
    inner_w: usize,
) {
    let bg = theme.code_bg;
    let pad = Style::default().bg(bg);
    let sign_w = 2usize; // `+ ` / `- ` / `  `
    let text_w = inner_w.saturating_sub(sign_w).max(1);

    for line in diff {
        let (sign, fg) = match line.op {
            DiffOp::Add => ('+', theme.success),
            DiffOp::Remove => ('-', theme.error_fg),
            DiffOp::Context => (' ', theme.dim_fg),
        };
        let wrapped = wrap_text(&line.text, text_w);
        let wrapped = if wrapped.is_empty() {
            vec![WrappedLine {
                text: String::new(),
                start_byte: 0,
                end_byte: 0,
            }]
        } else {
            wrapped
        };
        for (i, wl) in wrapped.iter().enumerate() {
            *content_lines += 1;
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= transcript_area.y + transcript_area.height {
                break;
            }
            // The sign only marks the first wrapped row; continuations align
            // under the text with a blank gutter.
            let gutter = if i == 0 {
                format!("{} ", sign)
            } else {
                "  ".to_string()
            };
            let used = indent + sign_w + wl.text.width();
            let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ".repeat(indent), pad),
                    Span::styled(gutter, Style::default().bg(bg).fg(fg)),
                    Span::styled(wl.text.clone(), Style::default().bg(bg).fg(fg)),
                    Span::styled(padded_tail(full_width, used), pad),
                ])),
                line_rect,
            );
            *current_y += 1;
        }
    }
}

/// Render a sub-agent `task` tool step as a compact, non-expandable card.
/// Activating it (click / Enter) navigates into a dedicated sub-agent view
/// rather than expanding a body inline. The card shows a one-line header
/// (the task description + duration) and a live status line summarizing the
/// sub-agent's progress.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_subagent_inline_card(
    frame: &mut Frame,
    transcript_area: Rect,
    msg: &TranscriptMessage,
    mi: usize,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    focused: bool,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };

    let status = msg
        .tool_step_status()
        .map(ToolStatus::from_status)
        .unwrap_or(ToolStatus::Running);
    let status_color = status.color(theme);

    let transcript_area = transcript_band_rect(transcript_area);
    let full_width = transcript_area.width as usize;
    if full_width < CARD_MIN_WIDTH {
        return;
    }

    let bg = theme.element_bg;
    let marker = match status {
        ToolStatus::Running => "▸",
        ToolStatus::Cancelled => "⊘",
        ToolStatus::Ok | ToolStatus::Failed => "✓",
    };
    let icon = match &msg.kind {
        crate::document::MessageKind::ToolStep { name, .. } => {
            super::tools::presenter_for(name).icon()
        }
        _ => ' ',
    };

    // Header band: status marker + tool icon + header text, registered as a
    // tool-step card header (block_idx = usize::MAX) so the existing
    // click/Enter handling recognizes it; the app decides to navigate rather
    // than toggle for `task` steps. No expand marker — the card navigates.
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < transcript_area.y + transcript_area.height {
        let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
        frame.render_widget(
            Paragraph::new(tool_header_line(
                "",
                marker,
                status_color,
                icon,
                &header,
                if focused {
                    theme.text
                } else {
                    theme.text_muted
                },
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
            if *current_y >= transcript_area.y + transcript_area.height {
                break;
            }
            let used = 2 + wl.text.width();
            let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
/// right. Drawn across the full transcript width inside the app_bg gutters.
pub(super) fn draw_subagent_bar(
    frame: &mut Frame,
    rect: Rect,
    bar: &SubagentBarInfo,
    theme: &Theme,
) {
    let band = transcript_band_rect(rect);
    let full_width = band.width as usize;
    if full_width < CARD_MIN_WIDTH {
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
pub(super) fn draw_tool_step_card(
    frame: &mut Frame,
    transcript_area: Rect,
    msg: &TranscriptMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    sticky_cards: &mut Vec<StickyCard>,
    spinner_phase: usize,
    focused: bool,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };

    // Status rail: a colored glyph in the header conveys run state at a glance
    // (spinner while running, `✓` success, `✗` failure, `⊘` cancelled). The
    // expand marker and tool icon stay muted so color reads purely as status.
    let status = msg
        .tool_step_status()
        .map(ToolStatus::from_status)
        .unwrap_or(ToolStatus::Running);
    let status_color = status.color(theme);
    let status_glyph: &str = match status {
        ToolStatus::Running => spinner_frame(spinner_phase),
        ToolStatus::Ok => "✓",
        ToolStatus::Failed => "✗",
        ToolStatus::Cancelled => "⊘",
    };
    let icon = match &msg.kind {
        crate::document::MessageKind::ToolStep { name, .. } => {
            super::tools::presenter_for(name).icon()
        }
        _ => ' ',
    };

    let expanded = msg.tool_step_expanded() == Some(true);
    // Render into the inset band so the `element_bg`/`menu_bg`/`code_bg` bands
    // never touch the terminal frame — they sit inside the uniform 2-cell
    // `app_bg` gutters shared with user panels and code blocks. All helpers
    // below (header, body sections, child tool steps) read `transcript_area.x` /
    // `transcript_area.width` directly, so shrinking here propagates everywhere.
    let transcript_area = transcript_band_rect(transcript_area);
    let full_width = transcript_area.width as usize;
    if full_width < CARD_MIN_WIDTH {
        // Too narrow to draw a card; fall back to plain block rendering.
        draw_message_body(
            frame,
            transcript_area,
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
    // border lines). Tool-step cards keep the shared card header treatment;
    // reasoning traces use their own prose-aligned header. `inner_width` is the
    // full band width; each body section subtracts its own indent before wrapping.
    let inner_width = transcript_area.width as usize;
    let header_line_idx = draw_expandable_card_header(
        frame,
        transcript_area,
        full_width,
        mi,
        usize::MAX,
        expanded,
        status_glyph,
        status_color,
        icon,
        &header,
        if focused {
            theme.text
        } else {
            theme.text_muted
        },
        theme.element_bg,
        layout_map,
        skip_rows,
        current_y,
        content_lines,
    );

    // Collapsed preview: under the header, lift a few lines of key content
    // (the `$ command` + output excerpt for bash, the first matches for grep,
    // …) so the user can see what a step did without expanding. Which lines
    // (if any) appear is owned by each tool's presenter; tools without a
    // preview keep the all-or-nothing collapsed header.
    if !expanded {
        if let crate::document::MessageKind::ToolStep {
            name,
            arguments,
            output: Some(output),
            ..
        } = &msg.kind
        {
            let preview = super::tools::collapsed_preview_for(name, arguments, output);
            if !preview.is_empty() {
                draw_tool_preview(
                    frame,
                    transcript_area,
                    full_width,
                    mi,
                    &preview,
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
        let command_style = Style::default().bg(body_bg).fg(theme.text);

        draw_blank_rows(
            frame,
            transcript_area,
            full_width,
            pad,
            TOOL_CARD_BODY_TOP_GAP_ROWS,
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
            // ── Arguments ── (only when the layout adds detail beyond the
            // header summary). No redundant `Tool` row — the header band
            // already identifies the tool via its icon + summary.
            match super::tools::presenter_for(name).arg_layout() {
                ArgLayout::None => {}
                ArgLayout::Command => {
                    let kv = crate::document::parse_arguments_kv(arguments);
                    let command = kv
                        .iter()
                        .find(|(k, _)| k == "command")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or_default();
                    draw_meta_value_row_wrapped(
                        frame,
                        transcript_area,
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
                }
                ArgLayout::KeyValue => {
                    let display_args: String = crate::document::parse_arguments_kv(arguments)
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k, v))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !display_args.is_empty() {
                        draw_tool_body_section(
                            frame,
                            transcript_area,
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
                }
            }

            // ── Result / Diff ── (only when output exists). The renderer is
            // chosen by the tool's `result_kind`: listings, grep matches,
            // command output, an edit/write diff, or a line-numbered code
            // fallback. The label sits on the body background; only the content
            // uses `code_bg`.
            if let Some(output_str) = output {
                if !output_str.is_empty() {
                    draw_tool_result_section(
                        frame,
                        transcript_area,
                        full_width,
                        mi,
                        name,
                        arguments,
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

        // ── Nested sub-agent children ── (blank-separated from Result).
        if let crate::document::MessageKind::ToolStep { children, .. } = &msg.kind {
            if !children.is_empty() {
                draw_blank_rows(
                    frame,
                    transcript_area,
                    full_width,
                    pad,
                    TOOL_CARD_CHILDREN_GAP_ROWS,
                    skip_rows,
                    current_y,
                    content_lines,
                );
            }
            for child in children {
                if child.is_tool_step() {
                    draw_child_tool_step(
                        frame,
                        transcript_area,
                        child,
                        status_color,
                        theme,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                } else {
                    let remaining_height = transcript_area
                        .y
                        .saturating_add(transcript_area.height)
                        .saturating_sub(*current_y);
                    let child_area = Rect::new(
                        transcript_area.x + 6,
                        *current_y,
                        transcript_area.width.saturating_sub(12),
                        remaining_height,
                    );
                    draw_message_body(
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

        draw_blank_rows(
            frame,
            transcript_area,
            full_width,
            pad,
            TOOL_CARD_BODY_BOTTOM_GAP_ROWS,
            skip_rows,
            current_y,
            content_lines,
        );
    }

    if expanded {
        sticky_cards.push(StickyCard {
            message_idx: mi,
            header,
            color: status_color,
            icon,
            background: Some(theme.element_bg),
            block_idx: usize::MAX,
            header_line: header_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// Render a nested child tool step as a compact header line plus its output.
#[allow(clippy::too_many_arguments)]
fn draw_child_tool_step(
    frame: &mut Frame,
    transcript_area: Rect,
    child: &TranscriptMessage,
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
    let full_width = transcript_area.width as usize;
    let indent = 6usize;

    let header_text = format!("⚒ {}", header);
    let header_lines = wrap_text(&header_text, full_width.saturating_sub(indent));
    *content_lines += header_lines.len();
    for wl in &header_lines {
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= transcript_area.y + transcript_area.height {
            break;
        }
        let used = indent + wl.text.width();
        let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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
            if *current_y >= transcript_area.y + transcript_area.height {
                break;
            }
            let used = indent + wl.text.width();
            let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
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

fn advance_plain_blank_rows(
    transcript_area: Rect,
    rows: usize,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    for _ in 0..rows {
        *content_lines += 1;
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
        } else if *current_y < transcript_area.y + transcript_area.height {
            *current_y += 1;
        }
    }
}

fn reasoning_trace_header_line(
    marker: &str,
    header: &str,
    marker_color: Color,
    header_color: Color,
    full_width: usize,
) -> Line<'static> {
    let marker_prefix_cols = TRANSCRIPT_H_INSET as usize;
    let marker_text = format!("{} ", marker);
    let header_text = header.to_string();
    let used = marker_prefix_cols + marker_text.width() + header_text.width();
    Line::from(vec![
        Span::styled(" ".repeat(marker_prefix_cols), Style::default()),
        Span::styled(
            marker_text,
            Style::default()
                .fg(marker_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            header_text,
            Style::default()
                .fg(header_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(padded_tail(full_width, used), Style::default()),
    ])
}

#[allow(clippy::too_many_arguments)]
fn draw_reasoning_trace_header(
    frame: &mut Frame,
    transcript_area: Rect,
    full_width: usize,
    mi: usize,
    expanded: bool,
    marker_override: Option<&str>,
    header: &str,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    hovered: bool,
) -> usize {
    let marker = marker_override.unwrap_or(if expanded { "-" } else { "+" });
    let header_line_idx = *content_lines;
    // Hover affordance: the muted header lights up to the primary foreground
    // (dark→bright) to signal the line is clickable.
    let header_color = if hovered {
        theme.text
    } else {
        theme.text_muted
    };

    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < transcript_area.y + transcript_area.height {
        let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
        frame.render_widget(
            Paragraph::new(reasoning_trace_header_line(
                marker,
                header,
                theme.info,
                header_color,
                full_width,
            )),
            line_rect,
        );
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX - 1,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: TRANSCRIPT_H_INSET,
            rect: line_rect,
        });
        *current_y += 1;
    }

    header_line_idx
}

/// Render a reasoning trace as expandable prose. It keeps the thinking
/// message model for stream semantics, but presents it as body-aligned text
/// instead of a colored card.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_reasoning_trace(
    frame: &mut Frame,
    transcript_area: Rect,
    msg: &TranscriptMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    sticky_cards: &mut Vec<StickyCard>,
    spinner_phase: usize,
    hovered: bool,
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
    let full_width = transcript_area.width as usize;

    if full_width < (TRANSCRIPT_BODY_PREFIX_COLS + 1) as usize {
        draw_message_body(
            frame,
            transcript_area,
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

    let header_line_idx = draw_reasoning_trace_header(
        frame,
        transcript_area,
        full_width,
        mi,
        expanded,
        running.then(|| spinner_frame(spinner_phase)),
        &header,
        theme,
        layout_map,
        skip_rows,
        current_y,
        content_lines,
        hovered,
    );

    if expanded {
        let body_prefix = " ".repeat(TRANSCRIPT_BODY_PREFIX_COLS as usize);
        let body_wrap_width = transcript_area
            .width
            .saturating_sub(TRANSCRIPT_BODY_PREFIX_COLS + TRANSCRIPT_BODY_RIGHT_INSET)
            as usize;

        advance_plain_blank_rows(
            transcript_area,
            REASONING_TRACE_BODY_TOP_GAP_ROWS,
            skip_rows,
            current_y,
            content_lines,
        );
        let mut emitted_any_block = false;
        for (bi, block) in msg.blocks.iter().enumerate() {
            if let Block::Text { content } = block {
                if emitted_any_block {
                    advance_plain_blank_rows(
                        transcript_area,
                        REASONING_TRACE_BLOCK_GAP_ROWS,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                }
                emitted_any_block = true;
                let lines = wrap_text(content, body_wrap_width);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= transcript_area.y + transcript_area.height {
                        break;
                    }
                    let sel_range = block_selection_range(selection, mi, bi);
                    let line = line_spans(
                        &body_prefix,
                        Style::default(),
                        &wl.text,
                        line_selection(sel_range, wl),
                        Style::default().fg(theme.text_muted),
                        theme.selected_bg,
                    );
                    let line_rect =
                        Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);
                    layout_map.push(BlockRegion {
                        message_idx: mi,
                        block_idx: bi,
                        start_byte: wl.start_byte,
                        end_byte: wl.end_byte,
                        text: wl.text.clone(),
                        prefix_cols: TRANSCRIPT_BODY_PREFIX_COLS,
                        rect: line_rect,
                    });
                    *current_y += 1;
                }
            }
        }
        advance_plain_blank_rows(
            transcript_area,
            REASONING_TRACE_BODY_BOTTOM_GAP_ROWS,
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
            icon: ' ',
            background: None,
            block_idx: usize::MAX - 1,
            header_line: header_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// Maximum number of output lines shown in the collapsed bash preview before
/// the `…` truncation marker kicks in.
/// Render a tool's collapsed-state preview (see [`super::tools::ToolPresenter::collapsed_preview`])
/// under the card header: each [`PreviewLine`] becomes one row in the body
/// background, hard-truncated to the inner width and colored by its tone. Rows
/// are registered with `block_idx = usize::MAX` so clicking anywhere on the
/// preview toggles the card open, matching the expandable-card interaction.
#[allow(clippy::too_many_arguments)]
fn draw_tool_preview(
    frame: &mut Frame,
    transcript_area: Rect,
    full_width: usize,
    mi: usize,
    lines: &[PreviewLine],
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

    for line in lines {
        *content_lines += 1;
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= transcript_area.y + transcript_area.height {
            break;
        }
        let fg = match line.tone {
            PreviewTone::Primary => theme.text,
            PreviewTone::Muted => theme.text_muted,
            PreviewTone::Faint => theme.dim_fg,
        };
        // Hard-truncate to the inner width (no per-line ellipsis) so the preview
        // height stays predictable; a trailing `…` row already signals "more".
        let text: String = line.text.chars().take(inner_w).collect();
        let used = indent + text.width();
        let line_rect = Rect::new(transcript_area.x, *current_y, transcript_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ".repeat(indent), pad),
                Span::styled(text, Style::default().bg(body_bg).fg(fg)),
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

/// If any expanded card's body covers the top of the viewport, render its
/// header pinned there as a sticky overlay and return its layout info so the
/// app can route clicks to it. Returns `None` when no sticky header is
/// needed.
pub(super) fn draw_sticky_header_if_needed(
    frame: &mut Frame,
    transcript_area: Rect,
    sticky_cards: &[StickyCard],
    scroll: u16,
    hovered_reasoning: Option<usize>,
    focused_target: Option<InteractiveTarget>,
    theme: &Theme,
) -> Option<StickyInfo> {
    let first_visible = scroll as usize;
    let card = sticky_cards
        .iter()
        .find(|c| c.header_line < first_visible && c.body_end_line > first_visible)?;
    // Reasoning sticky headers brighten on hover (dark→bright affordance),
    // matching the inline reasoning-trace header.
    let reasoning_hovered =
        card.background.is_none() && hovered_reasoning == Some(card.message_idx);
    let focused = focused_target.is_some_and(|target| {
        target.message_idx == card.message_idx
            && match target.kind {
                crate::layout::InteractiveTargetKind::ToolStep => {
                    card.block_idx == TOOL_STEP_BLOCK_IDX
                }
                crate::layout::InteractiveTargetKind::Thinking => {
                    card.block_idx == THINKING_BLOCK_IDX
                }
            }
    });
    let line_rect = if let Some(bg) = card.background {
        // Pin inside the same inset band the cards render into so the sticky
        // header aligns exactly with the (possibly scrolled-away) real header.
        let band = transcript_band_rect(transcript_area);
        let line_rect = Rect::new(band.x, transcript_area.y, band.width, 1);
        frame.render_widget(
            Paragraph::new(tool_header_line(
                "-",
                "",
                card.color,
                card.icon,
                &card.header,
                if focused {
                    theme.text
                } else {
                    theme.text_muted
                },
                bg,
                band.width as usize,
            )),
            line_rect,
        );
        line_rect
    } else {
        let line_rect = Rect::new(
            transcript_area.x,
            transcript_area.y,
            transcript_area.width,
            1,
        );
        let header_color = if reasoning_hovered || focused {
            theme.text
        } else {
            theme.text_muted
        };
        frame.render_widget(
            Paragraph::new(reasoning_trace_header_line(
                "-",
                &card.header,
                card.color,
                header_color,
                transcript_area.width as usize,
            )),
            line_rect,
        );
        line_rect
    };
    Some(StickyInfo {
        message_idx: card.message_idx,
        header: card.header.clone(),
        color: card.color,
        block_idx: card.block_idx,
        rect: line_rect,
        header_line: card.header_line,
    })
}
