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

use super::chrome::{breathing_color, spinner_glyph};
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

/// Cursor + environment carried through the tool-step body renderers.
///
/// Bundles the per-frame paint state (frame, viewport rect, scroll
/// accumulators, theme, layout map) so content renderers take a single
/// `&mut RenderCtx` plus their content-specific arguments, instead of 6-8
/// positional cursor args threaded through every helper. This is the
/// extraction seam for the tool-rendering redesign (ADR-0001); higher-level
/// orchestration still constructs a `RenderCtx` at the boundary.
pub(super) struct RenderCtx<'a, 'f: 'a> {
    pub frame: &'a mut Frame<'f>,
    pub area: Rect,
    pub full_width: usize,
    pub theme: &'a Theme,
    pub layout_map: &'a mut LayoutMap,
    pub skip_rows: &'a mut usize,
    pub y: &'a mut u16,
    pub content_lines: &'a mut usize,
}

impl<'a, 'f: 'a> RenderCtx<'a, 'f> {
    /// Assemble a render context from the raw cursor state owned by a caller.
    pub fn from_cursor(
        frame: &'a mut Frame<'f>,
        area: Rect,
        full_width: usize,
        theme: &'a Theme,
        layout_map: &'a mut LayoutMap,
        skip_rows: &'a mut usize,
        y: &'a mut u16,
        content_lines: &'a mut usize,
    ) -> Self {
        Self {
            frame,
            area,
            full_width,
            theme,
            layout_map,
            skip_rows,
            y,
            content_lines,
        }
    }

    /// Paint one already-built line at the cursor, honoring scroll-skip and
    /// viewport clip. Always accounts the row in `content_lines`, so callers
    /// must iterate every logical row even once the viewport is full —
    /// short-circuiting would undercount the scroll height. This reproduces
    /// the original "bulk-count then paint until clip" accounting per-row.
    ///
    /// Returns the painted `Rect` when the row was actually drawn (so callers
    /// can record a selectable [`BlockRegion`] for it), or `None` when the row
    /// was skipped or fell outside the viewport.
    pub fn paint(&mut self, line: Line<'static>) -> Option<Rect> {
        *self.content_lines += 1;
        if *self.skip_rows > 0 {
            *self.skip_rows = self.skip_rows.saturating_sub(1);
            return None;
        }
        if *self.y >= self.area.y + self.area.height {
            return None;
        }
        let rect = Rect::new(self.area.x, *self.y, self.area.width, 1);
        self.frame.render_widget(Paragraph::new(line), rect);
        *self.y += 1;
        Some(rect)
    }

    /// Paint `line` and, when drawn, record a selectable text region anchored
    /// at `wl`'s byte range under `(mi, block_idx)`. Collapses the per-row
    /// skip/clip/paint/record boilerplate that was duplicated across every
    /// content renderer.
    pub fn paint_text_row(
        &mut self,
        line: Line<'static>,
        mi: usize,
        block_idx: usize,
        wl: &WrappedLine,
        prefix_cols: u16,
    ) {
        if let Some(rect) = self.paint(line) {
            self.layout_map.push(BlockRegion {
                message_idx: mi,
                block_idx,
                start_byte: wl.start_byte,
                end_byte: wl.end_byte,
                text: wl.text.clone(),
                prefix_cols,
                rect,
            });
        }
    }
}


/// `WrappedLine::empty()`-on-empty fallback used by every content renderer so
/// a blank logical line still occupies one rendered row (matching the
/// original inline `if wrapped.is_empty() { vec![empty] } else { wrapped }`).
fn nonempty_wrapped(wrapped: Vec<WrappedLine>) -> Vec<WrappedLine> {
    if wrapped.is_empty() {
        vec![WrappedLine {
            text: String::new(),
            start_byte: 0,
            end_byte: 0,
        }]
    } else {
        wrapped
    }
}

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
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    expanded: bool,
    status_glyph: &str,
    status_color: Color,
    icon: char,
    header: &str,
    header_color: Color,
    bg: Color,
) -> usize {
    let expand = if expanded { "-" } else { "+" };
    let header_line_idx = *ctx.content_lines;

    let line = tool_header_line(
        expand,
        status_glyph,
        status_color,
        icon,
        header,
        header_color,
        bg,
        ctx.full_width,
    );
    if let Some(rect) = ctx.paint(line) {
        ctx.layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect,
        });
    }

    header_line_idx
}

/// Draw blank rows padded to `full_width` with `style`'s background. The row
/// count is supplied by component spacing tokens in `design.rs`.
fn draw_blank_rows(ctx: &mut RenderCtx<'_, '_>, style: Style, rows: usize) {
    for _ in 0..rows {
        let _ = ctx.paint(Line::from(Span::styled(padded_tail(ctx.full_width, 0), style)));
    }
}

/// Draw a section label line (" Tool", " Arguments", " Result") inside an
/// expanded card body. The label sits at column 1 (one-space prefix) in
/// `label_style`; the rest of the band is filled with `pad_style`'s
/// background so it reads as a solid colored row.
fn draw_section_label(
    ctx: &mut RenderCtx<'_, '_>,
    label: &str,
    pad_style: Style,
    label_style: Style,
) {
    let indent = 2usize;
    let used = indent + label.len();
    let line = Line::from(vec![
        Span::styled(" ".repeat(indent), pad_style),
        Span::styled(label.to_string(), label_style),
        Span::styled(padded_tail(ctx.full_width, used), pad_style),
    ]);
    let _ = ctx.paint(line);
}

/// Render a shell command line inside an expanded bash tool card. Long
/// commands wrap under the prompt so the expanded view stays compact without
/// losing the actual command that ran.
fn draw_meta_value_row_wrapped(
    ctx: &mut RenderCtx<'_, '_>,
    label: &str,
    value: &str,
    pad_style: Style,
    label_style: Style,
    value_style: Style,
    label_width: usize,
    inner_w: usize,
) {
    let value = value.trim_end();
    if value.is_empty() {
        return;
    }

    let indent = 2usize;
    let gap = 2usize;
    let value_indent = indent + label_width + gap;
    let wrap_w = inner_w.max(1);
    let wrapped = nonempty_wrapped(wrap_text(value, wrap_w));

    for (idx, wl) in wrapped.iter().enumerate() {
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
        spans.push(Span::styled(padded_tail(ctx.full_width, used), pad_style));
        let _ = ctx.paint(Line::from(spans));
    }
}

/// Render one labelled section (Arguments / a plain Result fallback) inside
/// an expanded tool-step card body. Handles scroll-skip, wrapping, semantic
/// selection layout recording, and an optional blank separator above the
/// label. Result rendering for known tools is handled by
/// [`draw_tool_result_section`] instead.
#[allow(clippy::too_many_arguments)]
fn draw_tool_body_section(
    ctx: &mut RenderCtx<'_, '_>,
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
) {
    if separator {
        draw_blank_rows(ctx, pad_style, TOOL_CARD_SECTION_GAP_ROWS);
    }

    draw_section_label(ctx, label, pad_style, label_style);

    // Content lines.
    let sel_range = block_selection_range(selection, mi, block_idx);
    if code_mode {
        draw_code_content(ctx, mi, block_idx, content, selection, indent, inner_w);
    } else {
        // Plain-text rendering: simple indent + wrap.
        let wrapped = nonempty_wrapped(wrap_text(content, inner_w));
        for wl in &wrapped {
            let block_wl = WrappedLine {
                text: wl.text.clone(),
                start_byte: wl.start_byte,
                end_byte: wl.end_byte,
            };
            let mut line = line_spans(
                &" ".repeat(indent),
                pad_style,
                &wl.text,
                line_selection(sel_range, &block_wl),
                content_style,
                ctx.theme.selected_bg,
            );
            let used = indent + wl.text.width();
            line.spans
                .push(Span::styled(padded_tail(ctx.full_width, used), pad_style));
            ctx.paint_text_row(line, mi, block_idx, &block_wl, indent as u16);
        }
    }
}

/// Render text content as a code block with a line-number gutter on
/// `code_bg`. Used for `read_file` / `edit_file` results and as the
/// fallback for unrecognized tools. The gutter starts at column `indent`
/// so the code aligns with the rest of the card body.
fn draw_code_content(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
) {
    let code_bg = ctx.theme.code_bg;
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
        let wrapped = nonempty_wrapped(wrap_text(logical_line, wrap_width));
        for (wrap_idx, wl) in wrapped.iter().enumerate() {
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
                ctx.theme.dim_fg,
                &wl.text,
                line_selection(sel_range, &block_wl),
                ctx.theme.code_fg,
                ctx.theme.selected_bg,
                ctx.full_width,
            );
            ctx.paint_text_row(line, mi, block_idx, &block_wl, gutter_indent as u16);
        }
    }
}

/// Render a `list_dir` / `glob` result: one entry per row on `code_bg`,
/// directories (entries ending in `/`) in `info`, files in `code_fg`. No
/// line-number gutter since listing rows have no meaningful line index.
fn draw_listing_content(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
) {
    let code_bg = ctx.theme.code_bg;
    let pad = Style::default().bg(code_bg);
    let dir_fg = ctx.theme.info;
    let file_fg = ctx.theme.code_fg;
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
        let wrapped = nonempty_wrapped(wrap_text(logical_line, wrap_w));
        for wl in &wrapped {
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
                ctx.theme.selected_bg,
            );
            let used = indent + wl.text.width();
            line.spans
                .push(Span::styled(padded_tail(ctx.full_width, used), pad));
            ctx.paint_text_row(line, mi, block_idx, &block_wl, indent as u16);
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
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    indent: usize,
    text: &str,
    abs_start: usize,
    pad: Style,
    style: Style,
    sel_range: Option<(usize, Option<usize>)>,
) {
    let wrap_w = ctx.full_width.saturating_sub(indent).max(1);
    let wrapped = nonempty_wrapped(wrap_text(text, wrap_w));
    for wl in &wrapped {
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
            ctx.theme.selected_bg,
        );
        let used = indent + wl.text.width();
        line.spans
            .push(Span::styled(padded_tail(ctx.full_width, used), pad));
        ctx.paint_text_row(line, mi, block_idx, &block_wl, indent as u16);
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
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    content: &str,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
) {
    let code_bg = ctx.theme.code_bg;
    let pad = Style::default().bg(code_bg);
    let header_style = Style::default()
        .bg(code_bg)
        .fg(ctx.theme.heading_fg)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().bg(code_bg).fg(ctx.theme.dim_fg);
    let match_style = Style::default().bg(code_bg).fg(ctx.theme.code_fg);
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
                        ctx,
                        mi,
                        block_idx,
                        indent,
                        parsed.path,
                        *line_start_byte,
                        pad,
                        header_style,
                        sel_range,
                    );
                }
                // Absolute byte offset of `content` within the tool output.
                let content_abs = line_start_byte + parsed.content_offset;
                let wrapped = nonempty_wrapped(wrap_text(parsed.content, content_wrap_w));
                for (wrap_idx, wl) in wrapped.iter().enumerate() {
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
                                match_style.bg(ctx.theme.selected_bg),
                            ));
                            if hi < wl.text.len() {
                                spans.push(Span::styled(wl.text[hi..].to_string(), match_style));
                            }
                        }
                    }
                    let used = content_cols + wl.text.width();
                    spans.push(Span::styled(padded_tail(ctx.full_width, used), pad));
                    ctx.paint_text_row(
                        Line::from(spans),
                        mi,
                        block_idx,
                        &block_wl,
                        content_cols as u16,
                    );
                }
            }
            None => {
                emit_simple_rows(
                    ctx,
                    mi,
                    block_idx,
                    indent,
                    logical_line,
                    *line_start_byte,
                    pad,
                    dim,
                    sel_range,
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
fn draw_bash_content(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    content: &str,
    structured: Option<&neenee_core::ToolOutput>,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
) {
    let result_bg = ctx.theme.menu_bg;
    let pad = Style::default().bg(result_bg);
    let base = Style::default().bg(result_bg).fg(ctx.theme.code_fg);
    let stderr_style = Style::default().bg(result_bg).fg(ctx.theme.error_fg);
    let marker_style = Style::default()
        .bg(result_bg)
        .fg(ctx.theme.warning)
        .add_modifier(Modifier::BOLD);
    let sel_range = block_selection_range(selection, mi, block_idx);
    let wrap_w = inner_w.saturating_sub(indent).max(1);

    if let Some(neenee_core::ToolOutput::Shell {
        stdout,
        stderr,
        exit,
        truncated,
        ..
    }) = structured
    {
        // Render from structured fields: stdout then stderr (distinguished by
        // color) then an exit/truncation footer — replacing the old sniffing
        // of `Exit N` / `STDERR:` markers embedded in the composed text.
        let mut byte_offset = 0usize;
        if !stdout.is_empty() {
            byte_offset = emit_bash_lines(
                ctx, mi, block_idx, indent, wrap_w, pad, sel_range, stdout, base, byte_offset,
            );
        }
        if !stderr.is_empty() {
            byte_offset = emit_bash_lines(
                ctx, mi, block_idx, indent, wrap_w, pad, sel_range, stderr, stderr_style,
                byte_offset,
            );
        }
        if *truncated {
            byte_offset = emit_bash_lines(
                ctx, mi, block_idx, indent, wrap_w, pad, sel_range, "[output truncated]",
                marker_style, byte_offset,
            );
        }
        if matches!(exit, Some(c) if *c != 0) {
            let m = format!("exit {}", exit.unwrap());
            let _ = emit_bash_lines(
                ctx, mi, block_idx, indent, wrap_w, pad, sel_range, &m, marker_style,
                byte_offset,
            );
        }
        return;
    }

    // Legacy fallback for non-Shell results (e.g. restored sessions whose
    // structured payload was not persisted): render the composed `content`
    // string, highlighting the conventional section markers.
    let content = content.trim_end_matches(&['\r', '\n'][..]);
    if content.is_empty() {
        return;
    }
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
        let _ = emit_bash_lines(
            ctx, mi, block_idx, indent, wrap_w, pad, sel_range, logical_line, style,
            *line_start_byte,
        );
    }
}

/// Emit a (possibly multi-line) bash body section at `indent`, wrapping to
/// `wrap_w`, all rows in `style`, anchoring selection byte ranges at
/// `*byte_offset` (advanced past the section). Shared by the structured and
/// legacy bash renderers.
fn emit_bash_lines(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    indent: usize,
    wrap_w: usize,
    pad: Style,
    sel_range: Option<(usize, Option<usize>)>,
    text: &str,
    style: Style,
    mut byte_offset: usize,
) -> usize {
    for logical_line in text.split('\n') {
        let wrapped = nonempty_wrapped(wrap_text(logical_line, wrap_w));
        for wl in &wrapped {
            let block_wl = WrappedLine {
                text: wl.text.clone(),
                start_byte: byte_offset + wl.start_byte,
                end_byte: byte_offset + wl.end_byte,
            };
            let mut line = line_spans(
                &" ".repeat(indent),
                pad,
                &wl.text,
                line_selection(sel_range, &block_wl),
                style,
                ctx.theme.selected_bg,
            );
            let used = indent + wl.text.width();
            line.spans
                .push(Span::styled(padded_tail(ctx.full_width, used), pad));
            ctx.paint_text_row(line, mi, block_idx, &block_wl, indent as u16);
        }
        byte_offset += logical_line.len() + 1;
    }
    byte_offset
}

/// Render the "Result" section of an expanded tool-step card. Draws the
/// blank separator and `Result` label on the surrounding body background
/// (so the label aligns with `Tool` / `Arguments`), then dispatches the
/// content rendering based on the tool name. Known tools with structured
/// output get a specialized renderer; everything else falls back to a
/// line-numbered code block via [`draw_code_content`].
#[allow(clippy::too_many_arguments)]
fn draw_tool_result_section(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    name: &str,
    arguments: &str,
    output: &str,
    structured: Option<&neenee_core::ToolOutput>,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
    separator: bool,
    body_pad: Style,
    body_label: Style,
) {
    if separator {
        draw_blank_rows(ctx, body_pad, TOOL_CARD_SECTION_GAP_ROWS);
    }
    let kind = super::tools::presenter_for(name).result_kind();
    // Diff steps relabel the section so it reads as a change, not raw output.
    let label = if kind == ResultKind::Diff {
        "Diff"
    } else {
        "Result"
    };
    draw_section_label(ctx, label, body_pad, body_label);

    let block_idx = 1usize;
    match kind {
        ResultKind::Listing => draw_listing_content(
            ctx,
            mi,
            block_idx,
            output,
            selection,
            indent,
            inner_w,
        ),
        ResultKind::Grep => draw_grep_content(
            ctx,
            mi,
            block_idx,
            output,
            selection,
            indent,
            inner_w,
        ),
        ResultKind::Bash => draw_bash_content(
            ctx,
            mi,
            block_idx,
            output,
            structured,
            selection,
            indent + 2,
            inner_w.saturating_sub(2),
        ),
        ResultKind::Code => draw_code_content(
            ctx,
            mi,
            block_idx,
            output,
            selection,
            indent,
            inner_w,
        ),
        ResultKind::Diff => {
            // Prefer the structured Patch payload (old/new from the result);
            // fall back to parsing the arguments for legacy/restored steps.
            let diff: Vec<DiffLine> = match structured {
                Some(neenee_core::ToolOutput::Patch { old, new, .. }) => {
                    super::tools::line_diff(old, new)
                }
                _ => super::tools::diff_lines_for(name, arguments),
            };
            draw_diff_content(ctx, &diff, indent, inner_w);
        }
    }
}

/// Render a red/green line diff inside an expanded edit/write card body. Each
/// [`DiffLine`] is a row in the `code_bg` block: a colored `+`/`-`/` ` sign
/// gutter then the (wrapped) line text. The diff is a derived view of the
/// tool's arguments, so rows aren't registered for text selection.
fn draw_diff_content(
    ctx: &mut RenderCtx<'_, '_>,
    diff: &[DiffLine],
    indent: usize,
    inner_w: usize,
) {
    let bg = ctx.theme.code_bg;
    let pad = Style::default().bg(bg);
    let gutter_fg = ctx.theme.dim_fg;
    // Gutter width from the widest 1-based line number, min 2 so single-digit
    // files still align with a leading space.
    let max_no = diff
        .iter()
        .filter_map(|l| l.old_no.or(l.new_no))
        .max()
        .unwrap_or(0);
    let gutter_w = max_no.to_string().len().max(2);
    let sign_w = 2usize; // "+ " / "- " / "  "
    let text_w = inner_w.saturating_sub(gutter_w + 1 + sign_w).max(1);
    // Tinted backgrounds for the changed word spans (low-chroma, Zen-palette
    // derived) so the exact edit reads without repainting whole lines.
    let add_hi_bg = Color::Rgb(40, 58, 45);
    let del_hi_bg = Color::Rgb(58, 38, 38);

    for line in diff {
        let (sign, base_fg, hi_bg) = match line.op {
            DiffOp::Add => ('+', ctx.theme.success, add_hi_bg),
            DiffOp::Remove => ('-', ctx.theme.error_fg, del_hi_bg),
            DiffOp::Context => (' ', ctx.theme.dim_fg, bg),
        };
        let no = line.old_no.or(line.new_no).unwrap_or(0);
        let gutter = format!("{:>width$} ", no, width = gutter_w);

        let full = line.text();
        let wrapped = nonempty_wrapped(wrap_text(&full, text_w));
        // Word-level highlighting only fits cleanly on a single rendered row;
        // wrapped (overflowing) lines fall back to plain base-color rows.
        let highlight_frags = wrapped.len() <= 1;

        for (i, wl) in wrapped.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = vec![
                Span::styled(" ".repeat(indent), pad),
                Span::styled(gutter.clone(), Style::default().bg(bg).fg(gutter_fg)),
                Span::styled(
                    if i == 0 {
                        format!("{} ", sign)
                    } else {
                        "  ".to_string()
                    },
                    Style::default().bg(bg).fg(base_fg),
                ),
            ];
            if highlight_frags && i == 0 {
                for frag in &line.frags {
                    let style = if frag.changed {
                        Style::default()
                            .bg(hi_bg)
                            .fg(base_fg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(bg).fg(base_fg)
                    };
                    spans.push(Span::styled(frag.text.clone(), style));
                }
            } else {
                spans.push(Span::styled(
                    wl.text.clone(),
                    Style::default().bg(bg).fg(base_fg),
                ));
            }
            let used = indent + gutter_w + 1 + sign_w + wl.text.width();
            spans.push(Span::styled(padded_tail(ctx.full_width, used), pad));
            let row = Line::from(spans);
            // Diff counts per-row and breaks on clip (distinct from the
            // bulk-counted content renderers), preserved verbatim.
            *ctx.content_lines += 1;
            if *ctx.skip_rows > 0 {
                *ctx.skip_rows = ctx.skip_rows.saturating_sub(1);
                continue;
            }
            if *ctx.y >= ctx.area.y + ctx.area.height {
                break;
            }
            let line_rect = Rect::new(ctx.area.x, *ctx.y, ctx.area.width, 1);
            ctx.frame.render_widget(Paragraph::new(row), line_rect);
            *ctx.y += 1;
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
    let header_color = if focused { theme.text } else { theme.text_muted };
    let mut ctx = RenderCtx::from_cursor(
        frame,
        transcript_area,
        full_width,
        theme,
        layout_map,
        skip_rows,
        current_y,
        content_lines,
    );
    let header_line = tool_header_line(
        "",
        marker,
        status_color,
        icon,
        &header,
        header_color,
        bg,
        ctx.full_width,
    );
    if let Some(rect) = ctx.paint(header_line) {
        ctx.layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect,
        });
    }

    // Live status line (e.g. "↳ Running: grep foo" / "↳ Completed · 3 calls").
    if let Some(status) = msg.subagent_status_line() {
        let inner_width = ctx.full_width.saturating_sub(2);
        let wrapped = wrap_text(&status, inner_width.max(1));
        let bg_style = Style::default().bg(bg);
        for wl in &wrapped {
            let used = 2 + wl.text.width();
            let line = Line::from(vec![
                Span::styled("  ", bg_style),
                Span::styled(wl.text.clone(), bg_style.fg(ctx.theme.text_muted)),
                Span::styled(padded_tail(ctx.full_width, used), bg_style),
            ]);
            // Make the whole status line part of the same clickable header so
            // clicking anywhere on the card enters the sub-agent view.
            if let Some(rect) = ctx.paint(line) {
                ctx.layout_map.push(BlockRegion {
                    message_idx: mi,
                    block_idx: usize::MAX,
                    start_byte: 0,
                    end_byte: 0,
                    text: String::new(),
                    prefix_cols: 0,
                    rect,
                });
            }
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
    let accent = Style::default().bg(bg).fg(theme.primary);

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
    let status_color = match status {
        // Breathing dot: luminance sweeps between the header bg and the status
        // color so a running step reads as "alive" without a frantic spinner.
        ToolStatus::Running => breathing_color(spinner_phase, status.color(theme), theme.element_bg),
        _ => status.color(theme),
    };
    let status_glyph: &str = match status {
        ToolStatus::Running => spinner_glyph(),
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

    let inner_width = transcript_area.width as usize;
    let header_line_idx = {
        let mut ctx = RenderCtx::from_cursor(
            frame,
            transcript_area,
            full_width,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
        );
        let idx = draw_expandable_card_header(
            &mut ctx,
            mi,
            usize::MAX,
            expanded,
            status_glyph,
            status_color,
            icon,
            &header,
            if focused { theme.text } else { theme.text_muted },
            theme.element_bg,
        );

        // Collapsed preview: under the header, lift a few lines of key content
        // (the `$ command` + output excerpt for bash, the first matches for
        // grep, …) so the user can see what a step did without expanding. While
        // a streaming tool is still running, the partial structured stdout is
        // used so a long bash command shows live output instead of freezing.
        if !expanded {
            let preview_output: Option<&str> = match &msg.kind {
                crate::document::MessageKind::ToolStep {
                    output: Some(output),
                    ..
                } => Some(output),
                crate::document::MessageKind::ToolStep {
                    name,
                    structured: Some(neenee_core::ToolOutput::Shell { stdout, .. }),
                    status: crate::document::ToolStepStatus::Running,
                    ..
                } if !stdout.is_empty() && name == "bash" => Some(stdout.as_str()),
                _ => None,
            };
            if let Some(output) = preview_output {
                let (name, arguments) = match &msg.kind {
                    crate::document::MessageKind::ToolStep { name, arguments, .. } => {
                        (name.as_str(), arguments.as_str())
                    }
                    _ => ("", ""),
                };
                let preview = super::tools::collapsed_preview_for(name, arguments, output);
                if !preview.is_empty() {
                    draw_tool_preview(&mut ctx, mi, &preview);
                }
            }
        }
        idx
    };

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

        {
            let mut ctx = RenderCtx::from_cursor(
                frame,
                transcript_area,
                full_width,
                theme,
                layout_map,
                skip_rows,
                current_y,
                content_lines,
            );
            draw_blank_rows(&mut ctx, pad, TOOL_CARD_BODY_TOP_GAP_ROWS);

            if let crate::document::MessageKind::ToolStep {
                name,
                arguments,
                output,
                structured,
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
                            &mut ctx,
                            "Arguments",
                            command,
                            pad,
                            meta_label_style,
                            command_style,
                            meta_label_width,
                            full_width.saturating_sub(indent + meta_label_width + 2),
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
                                &mut ctx,
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
                            );
                        }
                    }
                }

                // ── Result / Diff ── (only when output exists). The renderer is
                // chosen by the tool's `result_kind`: listings, grep matches,
                // command output, an edit/write diff, or a line-numbered code
                // fallback. The label sits on the body background; only the
                // content uses `code_bg`.
                if let Some(output_str) = output {
                    if !output_str.is_empty() {
                        draw_tool_result_section(
                            &mut ctx,
                            mi,
                            name,
                            arguments,
                            output_str,
                            structured.as_ref(),
                            selection,
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
                let mut ctx = RenderCtx::from_cursor(
                    frame,
                    transcript_area,
                    full_width,
                    theme,
                    layout_map,
                    skip_rows,
                    current_y,
                    content_lines,
                );
                draw_blank_rows(&mut ctx, pad, TOOL_CARD_CHILDREN_GAP_ROWS);
            }
            for child in children {
                if child.is_tool_step() {
                    let mut ctx = RenderCtx::from_cursor(
                        frame,
                        transcript_area,
                        full_width,
                        theme,
                        layout_map,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                    draw_child_tool_step(&mut ctx, child, status_color);
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

        {
            let mut ctx = RenderCtx::from_cursor(
                frame,
                transcript_area,
                full_width,
                theme,
                layout_map,
                skip_rows,
                current_y,
                content_lines,
            );
            draw_blank_rows(&mut ctx, pad, TOOL_CARD_BODY_BOTTOM_GAP_ROWS);
        }
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
    ctx: &mut RenderCtx<'_, '_>,
    child: &TranscriptMessage,
    status_color: Color,
) {
    let Some(header) = child.tool_step_header() else {
        return;
    };
    let body_bg = ctx.theme.menu_bg;
    let full_width = ctx.full_width;
    let indent = 6usize;
    let bg_style = Style::default().bg(body_bg);

    let header_text = format!("⚒ {}", header);
    let header_lines = wrap_text(&header_text, full_width.saturating_sub(indent));
    for wl in &header_lines {
        let used = indent + wl.text.width();
        let line = Line::from(vec![
            Span::styled(" ".repeat(indent), bg_style),
            Span::styled(wl.text.clone(), bg_style.fg(status_color)),
            Span::styled(padded_tail(full_width, used), bg_style),
        ]);
        let _ = ctx.paint(line);
    }

    if let crate::document::MessageKind::ToolStep {
        output: Some(output),
        ..
    } = &child.kind
    {
        let output_lines = wrap_text(output, full_width.saturating_sub(indent + 1));
        for wl in &output_lines {
            let used = indent + wl.text.width();
            let line = Line::from(vec![
                Span::styled(" ".repeat(indent), bg_style),
                Span::styled(wl.text.clone(), bg_style.fg(ctx.theme.text)),
                Span::styled(padded_tail(full_width, used), bg_style),
            ]);
            let _ = ctx.paint(line);
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

fn draw_reasoning_trace_header(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    expanded: bool,
    marker_override: Option<&str>,
    header: &str,
    hovered: bool,
) -> usize {
    let marker = marker_override.unwrap_or(if expanded { "-" } else { "+" });
    let header_line_idx = *ctx.content_lines;
    // Hover affordance: the muted header lights up to the primary foreground
    // (dark→bright) to signal the line is clickable.
    let header_color = if hovered { ctx.theme.text } else { ctx.theme.text_muted };

    let line = reasoning_trace_header_line(marker, header, ctx.theme.info, header_color, ctx.full_width);
    if let Some(rect) = ctx.paint(line) {
        ctx.layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX - 1,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: TRANSCRIPT_H_INSET,
            rect,
        });
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
    _spinner_phase: usize,
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

    let header_line_idx = {
        let mut ctx = RenderCtx::from_cursor(
            frame,
            transcript_area,
            full_width,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
        );
        draw_reasoning_trace_header(
            &mut ctx,
            mi,
            expanded,
            running.then(|| spinner_glyph()),
            &header,
            hovered,
        )
    };

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
                let mut ctx = RenderCtx::from_cursor(
                    frame,
                    transcript_area,
                    full_width,
                    theme,
                    layout_map,
                    skip_rows,
                    current_y,
                    content_lines,
                );
                let sel_range = block_selection_range(selection, mi, bi);
                for wl in &lines {
                    let block_wl = WrappedLine {
                        text: wl.text.clone(),
                        start_byte: wl.start_byte,
                        end_byte: wl.end_byte,
                    };
                    let line = line_spans(
                        &body_prefix,
                        Style::default(),
                        &wl.text,
                        line_selection(sel_range, &block_wl),
                        Style::default().fg(ctx.theme.text_muted),
                        ctx.theme.selected_bg,
                    );
                    let used = (TRANSCRIPT_BODY_PREFIX_COLS as usize) + wl.text.width();
                    let mut line = line;
                    line.spans
                        .push(Span::styled(padded_tail(ctx.full_width, used), Style::default()));
                    ctx.paint_text_row(line, mi, bi, &block_wl, TRANSCRIPT_BODY_PREFIX_COLS);
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
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    lines: &[PreviewLine],
) {
    let body_bg = ctx.theme.menu_bg;
    let pad = Style::default().bg(body_bg);
    let indent = 2usize;
    let inner_w = ctx.full_width.saturating_sub(indent).max(1);

    for line in lines {
        let fg = match line.tone {
            PreviewTone::Primary => ctx.theme.text,
            PreviewTone::Muted => ctx.theme.text_muted,
            PreviewTone::Faint => ctx.theme.dim_fg,
        };
        // Hard-truncate to the inner width (no per-line ellipsis) so the preview
        // height stays predictable; a trailing `…` row already signals "more".
        let text: String = line.text.chars().take(inner_w).collect();
        let used = indent + text.width();
        let row = Line::from(vec![
            Span::styled(" ".repeat(indent), pad),
            Span::styled(text, Style::default().bg(body_bg).fg(fg)),
            Span::styled(padded_tail(ctx.full_width, used), pad),
        ]);
        // Preview breaks on clip (preserved verbatim from the pre-ctx loop).
        *ctx.content_lines += 1;
        if *ctx.skip_rows > 0 {
            *ctx.skip_rows = ctx.skip_rows.saturating_sub(1);
            continue;
        }
        if *ctx.y >= ctx.area.y + ctx.area.height {
            break;
        }
        let line_rect = Rect::new(ctx.area.x, *ctx.y, ctx.area.width, 1);
        ctx.frame.render_widget(Paragraph::new(row), line_rect);
        ctx.layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: line_rect,
        });
        *ctx.y += 1;
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
