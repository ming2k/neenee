//! Expandable step renderers: tool-step, thinking, child tool step, sub-agent
//! task, plus their per-tool content renderers (code, listing, grep, bash) and
//! shared header helpers. Also produces the sticky pinned-step header that
//! [`super::draw_transcript`] overlays while a step body is scrolled into view.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::document::{Block, TranscriptMessage};
use crate::layout::{BlockRegion, LayoutMap};
use crate::selection::SelectionState;

use super::chrome::{breathing_color, spinner_glyph};
use super::message_body::draw_message_body;
use super::text_layout::{
    block_selection_range, code_gutter_line, line_selection, line_spans, padded_tail, wrap_text,
    WrappedLine,
};
use super::tools::{ArgLayout, DiffLine, DiffOp, ResultKind, ToolStatus};
use super::{
    transcript_band_rect, StickyInfo, SubagentBarInfo, Theme, STEP_MIN_WIDTH,
    REASONING_TRACE_BLOCK_GAP_ROWS, REASONING_TRACE_BODY_TOP_GAP_ROWS,
    TOOL_STEP_BODY_BOTTOM_GAP_ROWS, TOOL_STEP_BODY_TOP_GAP_ROWS,
    TOOL_STEP_CHILDREN_GAP_ROWS, TRANSCRIPT_BODY_PREFIX_COLS,
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

/// Tracked info for an expanded step, used to render a sticky header pinned
/// under the HUD bar while the step's body is scrolled into view.
pub(super) struct StickyStep {
    message_idx: usize,
    header: String,
    color: Color,
    background: Option<Color>,
    /// usize::MAX for tool steps, usize::MAX - 1 for reasoning traces.
    block_idx: usize,
    header_line: usize,
    body_end_line: usize,
}

/// Unified step-header foreground, shared by every step kind (tool step,
/// sub-agent inline step, reasoning trace) so they read consistently:
///
/// - **Expanded** (its body is open) → the primary foreground, the brightest
///   state, signalling "this is the active/open step".
/// - **Hovered** (pointer resting on the header while collapsed) → a distinct
///   intermediate tone, a softer click affordance than "open".
/// - **Idle** (collapsed, not hovered) → muted.
///
/// Notably a step that is merely *focused* (keyboard) but collapsed and not
/// hovered is **not** brightened — collapsing it visibly calms the header
/// instead of leaving it "still highlighted".
fn step_header_color(theme: &Theme, expanded: bool, hovered: bool) -> Color {
    if expanded {
        theme.fg()
    } else if hovered {
        theme.hover()
    } else {
        theme.muted()
    }
}

/// Build the header line of an expandable step: the `+`/`-` marker plus the
/// summary, padded to the full width. The body content is expected to start at
/// column 2 so it left-aligns with the header text.
///
/// Run state is conveyed purely by `header_color` (breathing accent while
/// running, error red on failure, muted when cancelled, neutral on success) —
/// there is no status glyph or per-tool icon in the header. An empty `expand`
/// segment (and its trailing space) is skipped so callers can omit it cleanly.
fn tool_header_line(
    expand: &str,
    header: &str,
    header_color: Color,
    bg: Color,
    full_width: usize,
) -> Line<'static> {
    let base = Style::default().bg(bg);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    let mut used = 0usize;

    if !expand.is_empty() {
        let s = format!("{} ", expand);
        used += s.width();
        spans.push(Span::styled(
            s,
            base.fg(header_color).add_modifier(Modifier::BOLD),
        ));
    }

    used += header.width();
    spans.push(Span::styled(
        header.to_string(),
        base.fg(header_color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(padded_tail(full_width, used), base));
    Line::from(spans)
}

/// Render the shared header of an expandable step and record its rect in the
/// layout map so clicks / `Enter` on it can toggle the step. Returns the
/// content-line index of the header (used for sticky-pin tracking).
///
/// `block_idx` is the sentinel recorded in [`BlockRegion`] so the click handler
/// can tell step/trace kinds apart: `usize::MAX` for tool steps and
/// `usize::MAX - 1` for reasoning traces.
#[allow(clippy::too_many_arguments)]
fn draw_expandable_step_header(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    expanded: bool,
    header: &str,
    header_color: Color,
    bg: Color,
) -> usize {
    let expand = if expanded { "-" } else { "+" };
    let header_line_idx = *ctx.content_lines;

    let line = tool_header_line(expand, header, header_color, bg, ctx.full_width);
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

/// Render text content as a code block with a line-number gutter on
/// `code_bg`. Used for `read_file` / `edit_file` results and as the
/// fallback for unrecognized tools. The gutter starts at column `indent`
/// so the code aligns with the rest of the step body.
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
                ctx.theme.dim(),
                &wl.text,
                line_selection(sel_range, &block_wl),
                ctx.theme.code_text(),
                ctx.theme.selected(),
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
    let dir_fg = ctx.theme.info();
    let file_fg = ctx.theme.code_text();
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
                ctx.theme.selected(),
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
            ctx.theme.selected(),
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
        .fg(ctx.theme.heading())
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().bg(code_bg).fg(ctx.theme.dim());
    let match_style = Style::default().bg(code_bg).fg(ctx.theme.code_text());
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
                                match_style.bg(ctx.theme.selected()),
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

/// Render a `bash` step as a terminal-like `code_bg` block: a `$ command`
/// prompt line first, then stdout / stderr (in `error_fg`) / an exit or
/// truncation footer. Output rows have no line-number gutter. Legacy section
/// markers (`Exit N`, `STDOUT:`, …) are highlighted in `warning` for sessions
/// restored without a structured payload. The command line is not selectable
/// (it's derived from the call, not the output stream); output rows are.
#[allow(clippy::too_many_arguments)]
fn draw_bash_content(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    content: &str,
    structured: Option<&neenee_core::ToolOutput>,
    command: &str,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
) {
    let result_bg = ctx.theme.code_bg;
    let pad = Style::default().bg(result_bg);
    let base = Style::default().bg(result_bg).fg(ctx.theme.code_text());
    let stderr_style = Style::default().bg(result_bg).fg(ctx.theme.err());
    let marker_style = Style::default()
        .bg(result_bg)
        .fg(ctx.theme.warn())
        .add_modifier(Modifier::BOLD);
    let sel_range = block_selection_range(selection, mi, block_idx);
    let wrap_w = inner_w.saturating_sub(indent).max(1);

    // `$ command` prompt line(s) — the command may span multiple lines; only
    // the first rendered row carries the `$ ` prompt.
    if !command.is_empty() {
        let cmd_style = Style::default().bg(result_bg).fg(ctx.theme.fg());
        let mut rows = command.split('\n');
        if let Some(first) = rows.next() {
            let prompt = format!("$ {}", first);
            for wl in nonempty_wrapped(wrap_text(&prompt, wrap_w)) {
                let used = indent + wl.text.width();
                let line = Line::from(vec![
                    Span::styled(" ".repeat(indent), pad),
                    Span::styled(wl.text.clone(), cmd_style),
                    Span::styled(padded_tail(ctx.full_width, used), pad),
                ]);
                let _ = ctx.paint(line);
            }
        }
        for cont in rows {
            for wl in nonempty_wrapped(wrap_text(cont, wrap_w)) {
                let used = indent + wl.text.width();
                let line = Line::from(vec![
                    Span::styled(" ".repeat(indent), pad),
                    Span::styled(wl.text.clone(), cmd_style),
                    Span::styled(padded_tail(ctx.full_width, used), pad),
                ]);
                let _ = ctx.paint(line);
            }
        }
    }

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
    // Shell capture appends a `\n` after every emitted line, so a payload like
    // `date`'s stdout (`"Fri … 2026\n"`) would otherwise split into
    // `["Fri … 2026", ""]` and paint a phantom trailing blank row (padded with
    // spaces). Trim trailing newlines first; internal blank lines are
    // preserved. This is a no-op for the single-line marker/legacy callers,
    // whose strings never carry a trailing newline.
    let text = text.trim_end_matches(&['\r', '\n'][..]);
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
                ctx.theme.selected(),
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

/// Render an expanded tool step's content — no `Result`/`Diff` label, no
/// separator; just the tool-specific block dispatched by `result_kind`. Known
/// tools with structured output get a specialized renderer; everything else
/// falls back to a line-numbered code block via [`draw_code_content`]. `bash`
/// additionally prefixes the block with a `$ command` line so the whole step
/// reads like a terminal session.
#[allow(clippy::too_many_arguments)]
fn draw_tool_result(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    name: &str,
    arguments: &str,
    output: &str,
    structured: Option<&neenee_core::ToolOutput>,
    selection: &SelectionState,
    indent: usize,
    inner_w: usize,
) {
    let kind = super::tools::presenter_for(name).result_kind();
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
        ResultKind::Bash => {
            let command = bash_command_for(structured, arguments);
            draw_bash_content(
                ctx,
                mi,
                block_idx,
                output,
                structured,
                &command,
                selection,
                indent,
                inner_w,
            );
        }
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

/// Resolve the shell command for a `bash` step: prefer the structured
/// [`ToolOutput::Shell`] payload (set as soon as the call starts, so it is
/// available even while streaming), falling back to parsing the JSON arguments
/// for legacy / restored sessions without a structured payload.
fn bash_command_for(
    structured: Option<&neenee_core::ToolOutput>,
    arguments: &str,
) -> String {
    if let Some(neenee_core::ToolOutput::Shell { command, .. }) = structured {
        if !command.is_empty() {
            return command.clone();
        }
    }
    crate::document::parse_arguments_kv(arguments)
        .iter()
        .find(|(k, _)| k == "command")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Render a red/green line diff inside an expanded edit/write step. Each
/// [`DiffLine`] is a row in the `code_bg` block: a colored `+`/`-`/` ` sign
/// gutter then the (wrapped) line text. The diff is a derived view of the
/// tool's arguments, so rows aren't registered for text selection.
fn draw_diff_content(
    ctx: &mut RenderCtx<'_, '_>,
    diff: &[DiffLine],
    indent: usize,
    inner_w: usize,
) {
    let code_bg = ctx.theme.code_bg;
    let gutter_fg = ctx.theme.muted();
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
    // opencode-style banding: the whole row carries a low-chroma tint so
    // added/removed blocks read at a glance, and the exact edited word sits
    // on a brighter tint on top of the row band. Context rows stay on the
    // neutral code surface so they recede.
    let add_row_bg = Color::Rgb(18, 31, 22);
    let del_row_bg = Color::Rgb(32, 20, 20);
    let add_hi_bg = Color::Rgb(42, 64, 48);
    let del_hi_bg = Color::Rgb(64, 40, 40);

    for line in diff {
        let (sign, row_bg, base_fg, hi_bg) = match line.op {
            DiffOp::Add => ('+', add_row_bg, ctx.theme.ok(), add_hi_bg),
            DiffOp::Remove => ('-', del_row_bg, ctx.theme.err(), del_hi_bg),
            DiffOp::Context => (' ', code_bg, ctx.theme.muted(), code_bg),
        };
        let pad = Style::default().bg(row_bg);
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
                Span::styled(gutter.clone(), Style::default().bg(row_bg).fg(gutter_fg)),
                Span::styled(
                    if i == 0 {
                        format!("{} ", sign)
                    } else {
                        "  ".to_string()
                    },
                    // The +/- sign carries the row's accent color and weight so
                    // the op reads from the gutter even without the row tint.
                    Style::default()
                        .bg(row_bg)
                        .fg(base_fg)
                        .add_modifier(Modifier::BOLD),
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
                        Style::default().bg(row_bg).fg(base_fg)
                    };
                    spans.push(Span::styled(frag.text.clone(), style));
                }
            } else {
                spans.push(Span::styled(
                    wl.text.clone(),
                    Style::default().bg(row_bg).fg(base_fg),
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

/// Render a sub-agent `task` tool step as a compact, non-expandable step.
/// Activating it (click / Enter) navigates into a dedicated sub-agent view
/// rather than expanding a body inline. The step shows a one-line header
/// (the task description + duration) and a live status line summarizing the
/// sub-agent's progress.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_subagent_inline_step(
    frame: &mut Frame,
    transcript_area: Rect,
    msg: &TranscriptMessage,
    mi: usize,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    hovered: bool,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };

    let status = msg
        .tool_step_status()
        .map(ToolStatus::from_status)
        .unwrap_or(ToolStatus::Running);

    let transcript_area = transcript_band_rect(transcript_area);
    let full_width = transcript_area.width as usize;
    if full_width < STEP_MIN_WIDTH {
        return;
    }

    let bg = theme.surface();

    // Header: just the summary text, registered as a tool-step header
    // (block_idx = usize::MAX) so the existing click/Enter handling recognizes
    // it; the app decides to navigate rather than toggle for `task` steps. No
    // expand marker or status glyph — the step navigates, and run state reads
    // from the header color (this step has no spinner phase, so a running step
    // is a steady accent rather than a breathing one). A finished step uses
    // the shared hover/idle ladder (it never expands inline), so a non-hovered
    // task reads calm and only lights up under the pointer.
    let header_color = match status {
        ToolStatus::Failed => theme.error_fg,
        ToolStatus::Denied => theme.warn(),
        ToolStatus::Cancelled => theme.text_muted,
        ToolStatus::Ok => step_header_color(theme, false, hovered),
        ToolStatus::Running => theme.info,
    };
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
    let header_line = tool_header_line("", &header, header_color, bg, ctx.full_width);
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
                Span::styled(wl.text.clone(), bg_style.fg(ctx.theme.muted())),
                Span::styled(padded_tail(ctx.full_width, used), bg_style),
            ]);
            // Make the whole status line part of the same clickable header so
            // clicking anywhere on the step enters the sub-agent view.
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
    if full_width < STEP_MIN_WIDTH {
        return;
    }
    let bg = theme.body();
    let muted = Style::default().bg(bg).fg(theme.muted());
    let label_style = Style::default()
        .bg(bg)
        .fg(theme.fg())
        .add_modifier(Modifier::BOLD);
    let accent = Style::default().bg(bg).fg(theme.brand());

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

/// Render a tool-step message as an expandable step with a summary header,
/// a body, and per-line scroll handling so tall steps scroll like
/// normal messages.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_tool_step(
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
    sticky_steps: &mut Vec<StickyStep>,
    spinner_phase: usize,
    hovered: bool,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };
    let expanded = msg.tool_step_expanded() == Some(true);

    // Run state is conveyed by color alone: a breathing accent while running,
    // red on failure, muted when cancelled, and neutral on success. There is no
    // status glyph or per-tool icon in the header. `status_color` drives the
    // child tool-step accents and the sticky pin; `header_color` drives the
    // header text. For the common success case the header follows the shared
    // expanded/hover/idle ladder so a finished call reads as calm: bright only
    // while its body is open or the pointer rests on it, never merely because
    // it carries keyboard focus.
    let status = msg
        .tool_step_status()
        .map(ToolStatus::from_status)
        .unwrap_or(ToolStatus::Running);
    // Tool steps render flat on the app background (no band) — like
    // reasoning traces, only the optional content block carries a `code_bg`.
    let header_bg = theme.surface();
    let status_color = match status {
        // Breathing accent: luminance sweeps between the header bg and the
        // status color so a running step reads as "alive" without a spinner.
        ToolStatus::Running => breathing_color(spinner_phase, status.color(theme), header_bg),
        _ => status.color(theme),
    };
    let header_color = match status {
        ToolStatus::Ok => step_header_color(theme, expanded, hovered),
        _ => status_color,
    };

    // Render into the inset band so content never touches the terminal frame —
    // it sits inside the uniform 2-cell `app_bg` gutters shared with prose and
    // code blocks. All helpers below (header, body, child tool steps) read
    // `transcript_area.x` / `transcript_area.width` directly, so shrinking here
    // propagates everywhere.
    let transcript_area = transcript_band_rect(transcript_area);
    let full_width = transcript_area.width as usize;
    if full_width < STEP_MIN_WIDTH {
        // Too narrow to draw; fall back to plain block rendering.
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
        draw_expandable_step_header(
            &mut ctx,
            mi,
            usize::MAX,
            expanded,
            &header,
            header_color,
            header_bg,
        )
    };

    // Body region (only when expanded). Tool steps are flat — no band, no
    // Tool/Arguments/Result labels — so an expanded step reads like a log entry:
    // the tool-specific content directly under the header (bash → `$ cmd` +
    // output; list/grep → entries; edit/write → diff; read → code), indented to
    // align with prose. Only content blocks carry a `code_bg`; everything else
    // sits on the app background.
    if expanded {
        let surface = theme.surface();
        let pad = Style::default().bg(surface);
        let indent = 2usize;
        let inner_w = inner_width.saturating_sub(indent);

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
            draw_blank_rows(&mut ctx, pad, TOOL_STEP_BODY_TOP_GAP_ROWS);

            if let crate::document::MessageKind::ToolStep {
                name,
                arguments,
                output,
                structured,
                ..
            } = &msg.kind
            {
                // Unknown / MCP tools spell out their arguments as `key: value`
                // rows (the header only carries the primary one). No label — the
                // key names are self-describing, and the result block below
                // carries its own `code_bg` so the two stay visually distinct.
                if matches!(
                    super::tools::presenter_for(name).arg_layout(),
                    ArgLayout::KeyValue
                ) {
                    let kv = crate::document::parse_arguments_kv(arguments);
                    if !kv.is_empty() {
                        let kv_style = Style::default().bg(surface).fg(theme.muted());
                        let wrap_w = inner_w.max(1);
                        for (k, v) in &kv {
                            let row = format!("{}: {}", k, v);
                            for wl in nonempty_wrapped(wrap_text(&row, wrap_w)) {
                                let used = indent + wl.text.width();
                                let line = Line::from(vec![
                                    Span::styled(" ".repeat(indent), pad),
                                    Span::styled(wl.text.clone(), kv_style),
                                    Span::styled(padded_tail(ctx.full_width, used), pad),
                                ]);
                                let _ = ctx.paint(line);
                            }
                        }
                    }
                }

                // Tool-specific content (label-free). bash renders `$ cmd` +
                // output; others their block. A streaming bash step may have no
                // composed output yet but a partial structured stdout.
                let has_output = output.as_deref().is_some_and(|s| !s.is_empty());
                let bash_streaming = matches!(
                    structured,
                    Some(neenee_core::ToolOutput::Shell { stdout, .. }) if !stdout.is_empty()
                );
                if has_output || bash_streaming {
                    draw_tool_result(
                        &mut ctx,
                        mi,
                        name,
                        arguments,
                        output.as_deref().unwrap_or(""),
                        structured.as_ref(),
                        selection,
                        indent,
                        inner_w,
                    );
                }
            }
        }

        // ── Nested sub-agent children ──.
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
                draw_blank_rows(&mut ctx, pad, TOOL_STEP_CHILDREN_GAP_ROWS);
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
            draw_blank_rows(&mut ctx, pad, TOOL_STEP_BODY_BOTTOM_GAP_ROWS);
        }
    }

    if expanded {
        sticky_steps.push(StickyStep {
            message_idx: mi,
            header,
            color: status_color,
            background: Some(theme.surface()),
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
    let surface = ctx.theme.surface();
    let full_width = ctx.full_width;
    let indent = 6usize;
    let bg_style = Style::default().bg(surface);

    let header_text = header.to_string();
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
                Span::styled(wl.text.clone(), bg_style.fg(ctx.theme.fg())),
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
    // Shared step-header ladder: an open trace reads as the primary foreground
    // (the active state), a collapsed trace under the pointer lights up to the
    // intermediate hover tone as a click affordance, and an idle collapsed
    // trace stays muted.
    let header_color = step_header_color(ctx.theme, expanded, hovered);

    let line = reasoning_trace_header_line(marker, header, ctx.theme.info(), header_color, ctx.full_width);
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
/// instead of a colored step.
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
    sticky_steps: &mut Vec<StickyStep>,
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
                        Style::default().fg(ctx.theme.muted()),
                        ctx.theme.selected(),
                    );
                    let used = (TRANSCRIPT_BODY_PREFIX_COLS as usize) + wl.text.width();
                    let mut line = line;
                    line.spans
                        .push(Span::styled(padded_tail(ctx.full_width, used), Style::default()));
                    ctx.paint_text_row(line, mi, bi, &block_wl, TRANSCRIPT_BODY_PREFIX_COLS);
                }
            }
        }
        // No trailing bottom gap here: the message-level separator
        // (`MESSAGE_GAP_ROWS`) already provides a single blank row between
        // this trace and the next component. Adding another would double the
        // gap when expanded, diverging from the collapsed layout.
    }

    if expanded {
        sticky_steps.push(StickyStep {
            message_idx: mi,
            header,
            color: theme.muted(),
            background: None,
            block_idx: usize::MAX - 1,
            header_line: header_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// If any expanded step's body covers the top of the viewport, render its
/// header pinned there as a sticky overlay and return its layout info so the
/// app can route clicks to it. Returns `None` when no sticky header is
/// needed.
///
/// A sticky header only exists for an *expanded* step (its body is what is
/// scrolled into view), so it always renders in the shared ladder's expanded
/// state — the primary foreground — matching the inline header of an open
/// step.
pub(super) fn draw_sticky_header_if_needed(
    frame: &mut Frame,
    transcript_area: Rect,
    sticky_steps: &[StickyStep],
    scroll: u16,
    theme: &Theme,
) -> Option<StickyInfo> {
    let first_visible = scroll as usize;
    let step = sticky_steps
        .iter()
        .find(|c| c.header_line < first_visible && c.body_end_line > first_visible)?;
    // Sticky steps are always expanded → the header reads in its active tone.
    let header_color = theme.fg();
    let line_rect = if let Some(bg) = step.background {
        // Pin inside the same inset band the steps render into so the sticky
        // header aligns exactly with the (possibly scrolled-away) real header.
        let band = transcript_band_rect(transcript_area);
        let line_rect = Rect::new(band.x, transcript_area.y, band.width, 1);
        frame.render_widget(
            Paragraph::new(tool_header_line(
                "-",
                &step.header,
                header_color,
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
        frame.render_widget(
            Paragraph::new(reasoning_trace_header_line(
                "-",
                &step.header,
                step.color,
                header_color,
                transcript_area.width as usize,
            )),
            line_rect,
        );
        line_rect
    };
    Some(StickyInfo {
        message_idx: step.message_idx,
        header: step.header.clone(),
        color: step.color,
        block_idx: step.block_idx,
        rect: line_rect,
        header_line: step.header_line,
    })
}
