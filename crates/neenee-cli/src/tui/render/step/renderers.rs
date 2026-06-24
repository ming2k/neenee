//! Step rendering implementation: the summary primitives, the per-tool body
//! content renderers (code, listing, grep, bash, diff), and the top-level
//! orchestrators (`draw_tool_step`, `draw_reasoning_trace`,
//! `draw_subagent_inline_step`, `draw_subagent_bar`) that compose them. Also
//! produces the sticky pinned-step summary that
//! [`super::super::draw_transcript`] overlays while a step body is scrolled
//! into view. State and color resolution live in [`super`] (re-exported from
//! [`super::state`]).

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::{summary_text_color, Disclosure, Interaction};

use crate::tui::document::{Block, TranscriptMessage};
use crate::tui::layout::{BlockRegion, LayoutMap};
use crate::tui::selection::SelectionState;

use crate::tui::render::message_body::draw_message_body;
use crate::tui::render::text_layout::{
    block_selection_range, code_gutter_line, line_selection, line_spans, padded_tail, wrap_text,
    WrappedLine,
};
use crate::tui::render::tools::{ArgLayout, DiffLine, DiffOp, ResultKind, ToolStatus};
use crate::tui::render::{
    transcript_band_rect, StickyInfo, SubagentBarInfo, Theme, REASONING_TRACE_BLOCK_GAP_ROWS,
    REASONING_TRACE_BODY_TOP_GAP_ROWS, STEP_MIN_WIDTH, TOOL_STEP_BODY_TOP_GAP_ROWS,
    TOOL_STEP_CHILDREN_GAP_ROWS, TRANSCRIPT_BODY_PREFIX_COLS, TRANSCRIPT_BODY_RIGHT_INSET,
    TRANSCRIPT_H_INSET,
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
    #[allow(clippy::too_many_arguments)]
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

/// Tracked info for an expanded step, used to render a sticky summary pinned
/// under the HUD bar while the step's body is scrolled into view.
pub struct StickyStep {
    message_idx: usize,
    summary: String,
    color: Color,
    background: Option<Color>,
    summary_line: usize,
    body_end_line: usize,
}

/// Build the summary line of an expandable step: the `+`/`-` marker plus the
/// summary text, padded to the full width. The body content is expected to
/// start at column 2 so it left-aligns with the summary text.
///
/// Run state is conveyed purely by `fg` (a steady `info` accent while running,
/// error red on failure, muted when cancelled, neutral on success) — there is
/// no status glyph or per-tool icon in the summary. An empty `expand` segment
/// (and its trailing space) is skipped so callers can omit it cleanly.
fn tool_summary_line(
    expand: &str,
    summary: &str,
    fg: Color,
    bg: Color,
    full_width: usize,
) -> Line<'static> {
    let base = Style::default().bg(bg);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    let mut used = 0usize;

    if !expand.is_empty() {
        let s = format!("{} ", expand);
        used += s.width();
        spans.push(Span::styled(s, base.fg(fg).add_modifier(Modifier::BOLD)));
    }

    used += summary.width();
    spans.push(Span::styled(
        summary.to_string(),
        base.fg(fg).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(padded_tail(full_width, used), base));
    Line::from(spans)
}

/// Render the shared summary of an expandable step and record its rect in the
/// layout map so clicks / `Enter` on it can toggle the step. Returns the
/// content-line index of the summary (used for sticky-pin tracking).
///
/// `block_idx` is the sentinel recorded in [`BlockRegion`] so the click handler
/// can tell step/trace kinds apart: `usize::MAX` for tool steps and
/// `usize::MAX - 1` for reasoning traces.
#[allow(clippy::too_many_arguments)]
fn draw_step_summary(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    expanded: bool,
    summary: &str,
    summary_color: Color,
    bg: Color,
) -> usize {
    let expand = if expanded { "-" } else { "+" };
    let summary_line_idx = *ctx.content_lines;

    let line = tool_summary_line(expand, summary, summary_color, bg, ctx.full_width);
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

    summary_line_idx
}

/// Draw blank rows padded to `full_width` with `style`'s background. The row
/// count is supplied by component spacing tokens in `design.rs`.
fn draw_blank_rows(ctx: &mut RenderCtx<'_, '_>, style: Style, rows: usize) {
    for _ in 0..rows {
        let _ = ctx.paint(Line::from(Span::styled(
            padded_tail(ctx.full_width, 0),
            style,
        )));
    }
}

/// Render text content as a code block with a line-number gutter on
/// `code_bg`. Used for `read_file` / `edit_file` results and as the
/// fallback for unrecognized tools. The gutter starts at column `indent`
/// so the code aligns with the rest of the step body.
///
/// `start_line` is the 1-based file line of the first row of `content`
/// (carried by `ToolOutput::Code::start_line`). `0` means "unknown" — the
/// renderer then numbers the slice 1, 2, 3… The gutter width is derived from
/// the *highest* displayed line number (not the line *count*) so an offset
/// snippet like 100..104 still gets a 3-wide column instead of overflowing.
#[allow(clippy::too_many_arguments)]
fn draw_code_content(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    block_idx: usize,
    content: &str,
    start_line: usize,
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
    // `0` (unknown) is indistinguishable from `1` for gutter purposes: both
    // render the first row as line 1. Normalize once so the math below is
    // uniform.
    let first_line = start_line.max(1);
    let last_line = first_line.saturating_add(logical_lines.len().saturating_sub(1));
    let gutter_width = last_line.to_string().len().max(2);
    let left_indent = indent;
    let gutter_gap = 1usize;
    let gutter_indent = left_indent + 1 /* space */ + gutter_width + gutter_gap;
    let wrap_width = inner_w.saturating_sub(1 + gutter_width + gutter_gap);
    let sel_range = block_selection_range(selection, mi, block_idx);

    for (line_idx, (line_start_byte, logical_line)) in logical_lines.iter().enumerate() {
        let wrapped = nonempty_wrapped(wrap_text(logical_line, wrap_width));
        for (wrap_idx, wl) in wrapped.iter().enumerate() {
            let gutter = if wrap_idx == 0 {
                format!("{:>width$}", first_line + line_idx, width = gutter_width)
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
                ctx,
                mi,
                block_idx,
                indent,
                wrap_w,
                pad,
                sel_range,
                stdout,
                base,
                byte_offset,
            );
        }
        if !stderr.is_empty() {
            byte_offset = emit_bash_lines(
                ctx,
                mi,
                block_idx,
                indent,
                wrap_w,
                pad,
                sel_range,
                stderr,
                stderr_style,
                byte_offset,
            );
        }
        if *truncated {
            byte_offset = emit_bash_lines(
                ctx,
                mi,
                block_idx,
                indent,
                wrap_w,
                pad,
                sel_range,
                "[output truncated]",
                marker_style,
                byte_offset,
            );
        }
        if let Some(code) = exit.filter(|c| *c != 0) {
            let m = format!("exit {}", code);
            let _ = emit_bash_lines(
                ctx,
                mi,
                block_idx,
                indent,
                wrap_w,
                pad,
                sel_range,
                &m,
                marker_style,
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
            ctx,
            mi,
            block_idx,
            indent,
            wrap_w,
            pad,
            sel_range,
            logical_line,
            style,
            *line_start_byte,
        );
    }
}

/// Emit a (possibly multi-line) bash body section at `indent`, wrapping to
/// `wrap_w`, all rows in `style`, anchoring selection byte ranges at
/// `*byte_offset` (advanced past the section). Shared by the structured and
/// legacy bash renderers.
#[allow(clippy::too_many_arguments)]
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
    let kind = crate::tui::render::tools::presenter_for(name).result_kind();
    let block_idx = 1usize;
    match kind {
        ResultKind::Listing => {
            draw_listing_content(ctx, mi, block_idx, output, selection, indent, inner_w)
        }
        ResultKind::Grep => {
            draw_grep_content(ctx, mi, block_idx, output, selection, indent, inner_w)
        }
        ResultKind::Bash => {
            let command = bash_command_for(structured, arguments);
            draw_bash_content(
                ctx, mi, block_idx, output, structured, &command, selection, indent, inner_w,
            );
        }
        ResultKind::Code => {
            // Prefer the structured payload: its `text` is pure file content
            // (the model-facing `prefix`/`suffix` framing is ignored here) and
            // `start_line` carries the read `offset` so an offset snippet
            // numbers from its true file line. Legacy/restored steps without a
            // payload fall back to the flattened `output` string with
            // `start_line = 0` (slice-relative 1-based numbering).
            let (content, start_line) = match structured {
                Some(neenee_core::ToolOutput::Code {
                    text, start_line, ..
                }) => (text.as_str(), *start_line),
                _ => (output, 0),
            };
            draw_code_content(
                ctx, mi, block_idx, content, start_line, selection, indent, inner_w,
            )
        }
        ResultKind::Diff => {
            // Prefer the structured Patch payload (old/new from the result);
            // fall back to parsing the arguments for legacy/restored steps.
            let diff: Vec<DiffLine> = match structured {
                Some(neenee_core::ToolOutput::Patch {
                    old,
                    new,
                    start_line,
                    ..
                }) => {
                    let offset = start_line.saturating_sub(1);
                    let full = crate::tui::render::tools::line_diff(old, new, offset);
                    crate::tui::render::tools::collapse_context_runs(&full)
                }
                _ => {
                    let full = crate::tui::render::tools::diff_lines_for(name, arguments);
                    crate::tui::render::tools::collapse_context_runs(&full)
                }
            };
            draw_diff_content(ctx, &diff, indent, inner_w);
        }
    }
}

/// Resolve the shell command for a `bash` step: prefer the structured
/// [`ToolOutput::Shell`](neenee_core::ToolOutput) payload (set as soon as the
/// call starts, so it is available even while streaming), falling back to
/// parsing the JSON arguments for legacy / restored sessions without a
/// structured payload.
fn bash_command_for(structured: Option<&neenee_core::ToolOutput>, arguments: &str) -> String {
    if let Some(neenee_core::ToolOutput::Shell { command, .. }) = structured {
        if !command.is_empty() {
            return command.clone();
        }
    }
    crate::tui::document::parse_arguments_kv(arguments)
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
            DiffOp::Ellipsis => (' ', code_bg, ctx.theme.muted(), code_bg),
        };
        let pad = Style::default().bg(row_bg);
        let gutter = if line.op == DiffOp::Ellipsis {
            format!("{:>width$} ", "⋯", width = gutter_w)
        } else {
            let no = line.old_no.or(line.new_no).unwrap_or(0);
            format!("{:>width$} ", no, width = gutter_w)
        };

        // Ellipsis rows: just the marker, no text or word highlighting.
        if line.op == DiffOp::Ellipsis {
            let mut spans: Vec<Span<'static>> = vec![
                Span::styled(" ".repeat(indent), pad),
                Span::styled(gutter, Style::default().bg(row_bg).fg(gutter_fg)),
                Span::styled("  ", Style::default().bg(row_bg)),
                Span::styled("⋯", Style::default().bg(row_bg).fg(gutter_fg)),
            ];
            let used = indent + gutter_w + 1 + sign_w + 1;
            spans.push(Span::styled(padded_tail(ctx.full_width, used), pad));
            *ctx.content_lines += 1;
            if *ctx.skip_rows > 0 {
                *ctx.skip_rows = ctx.skip_rows.saturating_sub(1);
                continue;
            }
            if *ctx.y >= ctx.area.y + ctx.area.height {
                break;
            }
            let line_rect = Rect::new(ctx.area.x, *ctx.y, ctx.area.width, 1);
            ctx.frame
                .render_widget(Paragraph::new(Line::from(spans)), line_rect);
            *ctx.y += 1;
            continue;
        }

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

/// Render a subagent `task` tool step as a compact, non-expandable step.
/// Activating it (click / Enter) navigates into a dedicated subagent view
/// rather than expanding a body inline. The step shows a one-line summary
/// (the task description + duration) and a live status line summarizing the
/// subagent's progress.
#[allow(clippy::too_many_arguments)]
pub fn draw_subagent_inline_step(
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
    let Some(summary) = msg.tool_step_summary() else {
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

    // Summary line: just the summary text, registered as a tool-step summary
    // (block_idx = usize::MAX) so the existing click/Enter handling recognizes
    // it; the app decides to navigate rather than toggle for `task` steps. No
    // expand marker or status glyph — the step navigates, and run state reads
    // from the summary color (a steady accent, matching every other step per
    // the single-breathing-anchor rule in ADR 0008). Color is resolved through
    // the shared state machine: a non-completed lifecycle supplies an accent
    // that wins outright; the completed case falls through to the disclosure ×
    // interaction weight ladder (a task never expands inline, so it is bright
    // only under the pointer and calm otherwise).
    let accent = match status {
        ToolStatus::Failed => Some(theme.error_fg),
        ToolStatus::Denied => Some(theme.warn()),
        ToolStatus::Cancelled => Some(theme.text_muted),
        ToolStatus::Ok => None,
        ToolStatus::Running => Some(theme.info),
    };
    let summary_color = summary_text_color(
        accent,
        Disclosure::Collapsed,
        Interaction::from_hover(hovered),
        theme,
    );
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
    let summary_row = tool_summary_line("", &summary, summary_color, bg, ctx.full_width);
    if let Some(rect) = ctx.paint(summary_row) {
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
            // Make the whole status line part of the same clickable summary so
            // clicking anywhere on the step enters the subagent view.
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

/// Render the subagent navigation bar: the focused task's label + position
/// among siblings on the left, and the return / cycle-sibling hints on the
/// right. Drawn across the full transcript width inside the app_bg gutters.
pub fn draw_subagent_bar(frame: &mut Frame, rect: Rect, bar: &SubagentBarInfo, theme: &Theme) {
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

/// Render the `/btw` side banner (ADR-0017): a top band reading
/// `Side from main · <parent status> · Esc back`. Mirrors `draw_subagent_bar`'s
/// style palette so the two zoom modes share a visual language; the parent
/// status segment is accented so the user notices when the main session hits a
/// running / idle transition.
pub fn draw_side_banner(
    frame: &mut Frame,
    rect: Rect,
    status: neenee_core::ParentStatus,
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

    let left_label = " Side from main ".to_string();
    let status_label = match status {
        neenee_core::ParentStatus::Idle => "main idle",
        neenee_core::ParentStatus::Running => "main running",
        neenee_core::ParentStatus::NeedsApproval => "main needs approval",
        neenee_core::ParentStatus::NeedsInput => "main needs input",
        neenee_core::ParentStatus::Failed => "main failed",
        neenee_core::ParentStatus::Interrupted => "main interrupted",
    };
    let sep = " · ";
    let right = " Esc back ".to_string();

    let left_used = left_label.width() + sep.width() + status_label.width();
    let gap = full_width.saturating_sub(left_used + right.width());
    let mut spans = vec![
        Span::styled(left_label, label_style),
        Span::styled(sep, muted),
        Span::styled(status_label, accent),
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

/// Render a tool-step message as an expandable step with a summary line,
/// a body, and per-line scroll handling so tall steps scroll like
/// normal messages.
#[allow(clippy::too_many_arguments)]
pub fn draw_tool_step(
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
    hovered: bool,
) {
    let Some(summary) = msg.tool_step_summary() else {
        return;
    };
    let expanded = msg.tool_step_expanded() == Some(true);

    // Run state is conveyed by color alone: a steady `info` accent while
    // running, red on failure, muted when cancelled, and neutral on success.
    // There is no status glyph or per-tool icon in the summary. The summary
    // text color is resolved through the shared state machine: a non-completed
    // lifecycle supplies an accent that wins outright; the completed case falls
    // through to the disclosure × interaction weight ladder so a finished call
    // reads as calm — bright only while its body is open or the pointer rests
    // on it, never merely because it carries keyboard focus.
    //
    // The activity bar is the single breathing anchor (ADR 0008); per-step
    // liveness rides on hue alone so a transcript full of running steps does
    // not flash in unison and steal attention from the content the user is
    // reading.
    let status = msg
        .tool_step_status()
        .map(ToolStatus::from_status)
        .unwrap_or(ToolStatus::Running);
    // Tool steps render flat on the app background (no band) — like
    // reasoning traces, only the optional content block carries a `code_bg`.
    let summary_bg = theme.surface();
    let status_color = status.color(theme);
    let accent = match status {
        ToolStatus::Ok => None,
        _ => Some(status_color),
    };
    let summary_color = summary_text_color(
        accent,
        Disclosure::from_expanded(expanded),
        Interaction::from_hover(hovered),
        theme,
    );

    // Render into the inset band so content never touches the terminal frame —
    // it sits inside the uniform 2-cell `app_bg` gutters shared with prose and
    // code blocks. All helpers below (summary, body, child tool steps) read
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
    let summary_line_idx = {
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
        draw_step_summary(
            &mut ctx,
            mi,
            usize::MAX,
            expanded,
            &summary,
            summary_color,
            summary_bg,
        )
    };

    // Body region (only when expanded). Tool steps are flat — no band, no
    // Tool/Arguments/Result labels — so an expanded step reads like a log entry:
    // the tool-specific content directly under the summary (bash → `$ cmd` +
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

            if let crate::tui::document::MessageKind::ToolStep {
                name,
                arguments,
                output,
                structured,
                ..
            } = &msg.kind
            {
                // Unknown / MCP tools spell out their arguments as `key: value`
                // rows (the summary only carries the primary one). No label — the
                // key names are self-describing, and the result block below
                // carries its own `code_bg` so the two stay visually distinct.
                if matches!(
                    crate::tui::render::tools::presenter_for(name).arg_layout(),
                    ArgLayout::KeyValue
                ) {
                    let kv = crate::tui::document::parse_arguments_kv(arguments);
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
                    structured.as_deref(),
                    Some(neenee_core::ToolOutput::Shell { stdout, .. }) if !stdout.is_empty()
                );
                if has_output || bash_streaming {
                    draw_tool_result(
                        &mut ctx,
                        mi,
                        name,
                        arguments,
                        output.as_deref().unwrap_or(""),
                        structured.as_deref(),
                        selection,
                        indent,
                        inner_w,
                    );
                }
            }
        }

        // ── Nested subagent children ──.
        if let crate::tui::document::MessageKind::ToolStep { children, .. } = &msg.kind {
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

        // No trailing bottom gap here: the message-level separator
        // (`MESSAGE_GAP_ROWS`) already provides a single blank row between
        // this step and the next component. Adding another would double the
        // gap when expanded, diverging from the collapsed layout.
    }

    if expanded {
        sticky_steps.push(StickyStep {
            message_idx: mi,
            summary,
            color: status_color,
            background: Some(theme.surface()),
            summary_line: summary_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// Render a nested child tool step as a compact summary line plus its output.
#[allow(clippy::too_many_arguments)]
fn draw_child_tool_step(
    ctx: &mut RenderCtx<'_, '_>,
    child: &TranscriptMessage,
    status_color: Color,
) {
    let Some(summary) = child.tool_step_summary() else {
        return;
    };
    let surface = ctx.theme.surface();
    let full_width = ctx.full_width;
    let indent = 6usize;
    let bg_style = Style::default().bg(surface);

    let summary_text = summary.to_string();
    let summary_lines = wrap_text(&summary_text, full_width.saturating_sub(indent));
    for wl in &summary_lines {
        let used = indent + wl.text.width();
        let line = Line::from(vec![
            Span::styled(" ".repeat(indent), bg_style),
            Span::styled(wl.text.clone(), bg_style.fg(status_color)),
            Span::styled(padded_tail(full_width, used), bg_style),
        ]);
        let _ = ctx.paint(line);
    }

    if let crate::tui::document::MessageKind::ToolStep {
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

fn reasoning_summary_line(
    marker: &str,
    summary: &str,
    marker_color: Color,
    summary_color: Color,
    full_width: usize,
) -> Line<'static> {
    let marker_prefix_cols = TRANSCRIPT_H_INSET as usize;
    let marker_text = format!("{} ", marker);
    let summary_text = summary.to_string();
    let used = marker_prefix_cols + marker_text.width() + summary_text.width();
    Line::from(vec![
        Span::styled(" ".repeat(marker_prefix_cols), Style::default()),
        Span::styled(
            marker_text,
            Style::default()
                .fg(marker_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            summary_text,
            Style::default()
                .fg(summary_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(padded_tail(full_width, used), Style::default()),
    ])
}

fn draw_reasoning_summary(
    ctx: &mut RenderCtx<'_, '_>,
    mi: usize,
    expanded: bool,
    marker_override: Option<&str>,
    summary: &str,
    marker_color: Color,
    hovered: bool,
) -> usize {
    let marker = marker_override.unwrap_or(if expanded { "-" } else { "+" });
    let summary_line_idx = *ctx.content_lines;
    // A reasoning trace's lifecycle is carried by the summary text (duration
    // omitted while streaming) and the steady `info` hue — never by the
    // marker, which is always the disclosure `+`/`-`. So no accent is
    // supplied and the summary color is the pure disclosure × interaction
    // weight from the shared state machine: open → primary foreground,
    // collapsed + hovered → intermediate hover tone, otherwise muted.
    let summary_color = summary_text_color(
        None,
        Disclosure::from_expanded(expanded),
        Interaction::from_hover(hovered),
        ctx.theme,
    );

    let line = reasoning_summary_line(marker, summary, marker_color, summary_color, ctx.full_width);
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

    summary_line_idx
}

/// Render a reasoning trace as expandable prose. It keeps the thinking
/// message model for stream semantics, but presents it as body-aligned text
/// instead of a colored step.
#[allow(clippy::too_many_arguments)]
pub fn draw_reasoning_trace(
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
    hovered: bool,
) {
    let Some(summary) = msg.thinking_summary() else {
        return;
    };
    let expanded = msg.thinking_expanded() == Some(true);
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

    let summary_line_idx = {
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
        draw_reasoning_summary(
            &mut ctx,
            mi,
            expanded,
            // Always use the disclosure marker (`+`/`-`), never a streaming
            // `●`. With the activity bar as the single breathing anchor
            // (ADR 0008), nothing about the marker needs to change between
            // streaming and finished — the lifecycle reads from the summary
            // text (duration omitted while streaming) and the steady `info`
            // hue alone.
            None,
            &summary,
            theme.info(),
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
                    line.spans.push(Span::styled(
                        padded_tail(ctx.full_width, used),
                        Style::default(),
                    ));
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
            summary,
            color: theme.muted(),
            background: None,
            summary_line: summary_line_idx,
            body_end_line: *content_lines,
        });
    }
}

/// If any expanded step's body covers the top of the viewport, render its
/// summary pinned there as a sticky overlay and return its layout info so the
/// app can route clicks to it. Returns `None` when no sticky summary is
/// needed.
///
/// A sticky summary only exists for an *expanded* step (its body is what is
/// scrolled into view), so it always renders in the shared ladder's expanded
/// state — the primary foreground — matching the inline summary of an open
/// step.
pub fn draw_sticky_summary_if_needed(
    frame: &mut Frame,
    transcript_area: Rect,
    sticky_steps: &[StickyStep],
    scroll: u16,
    theme: &Theme,
) -> Option<StickyInfo> {
    let first_visible = scroll as usize;
    let step = sticky_steps
        .iter()
        .find(|c| c.summary_line < first_visible && c.body_end_line > first_visible)?;
    // Sticky steps are always expanded → the summary reads in its active tone.
    let summary_color = theme.fg();
    let line_rect = if let Some(bg) = step.background {
        // Pin inside the same inset band the steps render into so the sticky
        // summary aligns exactly with the (possibly scrolled-away) real summary.
        let band = transcript_band_rect(transcript_area);
        let line_rect = Rect::new(band.x, transcript_area.y, band.width, 1);
        frame.render_widget(
            Paragraph::new(tool_summary_line(
                "-",
                &step.summary,
                summary_color,
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
            Paragraph::new(reasoning_summary_line(
                "-",
                &step.summary,
                step.color,
                summary_color,
                transcript_area.width as usize,
            )),
            line_rect,
        );
        line_rect
    };
    Some(StickyInfo {
        message_idx: step.message_idx,
        rect: line_rect,
        summary_line: step.summary_line,
    })
}
