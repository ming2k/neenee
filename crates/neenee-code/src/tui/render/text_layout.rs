//! Low-level text helpers: wrapping, byte-range → span splitting, code-line
//! gutter rendering, and selection arithmetic shared by every renderer that
//! lays out character-addressable content.

use neenee_tui::{
    {Color, Style}, {Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::selection::{SelectionState, floor_char_boundary, inclusive_end};

/// Produce a run of spaces that fills the rest of a full-width line so a
/// region reads as a solid colored band (the caller attaches the bg style).
pub(super) fn padded_tail(full_width: usize, used: usize) -> String {
    " ".repeat(full_width.saturating_sub(used))
}

/// The byte range of a block covered by the selection.
/// `(start, None)` means "from start to the end of the block".
pub(super) fn block_selection_range(
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
pub(super) fn line_selection(
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
pub(super) fn line_spans(
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

/// Build a rendered line that, in addition to the prefix / selection behaviour
/// of [`line_spans`], paints inline-code runs on a distinct `code_bg` band.
///
/// `text` is the flattened text for this wrapped line; `line_start_byte` is its
/// byte offset within the block's raw `content` (so that `code_ranges`, which
/// are absolute byte ranges into `content`, can be intersected with this line).
/// `selected` is relative to `text`, as in [`line_spans`].
///
/// The line is split into runs that are each uniform in *both* the selection
/// state and the code state, so every cell is painted with exactly one of:
/// plain (`base`), selected plain (`base` + `selected_bg`), code
/// (`code_fg` + `code_bg`), or selected code (`code_fg` + `selected_bg`). The
/// backtick delimiters are part of the code run (they live inside the recorded
/// range), so the rendered chip reads as `` `read_file` `` on the code surface
/// — and copy still yields the exact source, because the underlying text is
/// untouched.
#[allow(clippy::too_many_arguments)]
pub(super) fn line_spans_rich(
    prefix: &str,
    prefix_style: Style,
    text: &str,
    line_start_byte: usize,
    selected: Option<(usize, usize)>,
    code_ranges: &[(usize, usize)],
    base: Style,
    code_fg: Color,
    code_bg: Color,
    selected_bg: Color,
) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_string(), prefix_style)];
    if text.is_empty() {
        return Line::from(spans);
    }

    // Code ranges translated into offsets relative to this line's text, clamped
    // to the line's own [0, len] window.
    let line_end_byte = line_start_byte + text.len();
    let mut local_code: Vec<(usize, usize)> = Vec::new();
    for &(cs, ce) in code_ranges {
        if ce <= line_start_byte || cs >= line_end_byte {
            continue;
        }
        let lo = cs.saturating_sub(line_start_byte);
        let hi = (ce - line_start_byte).min(text.len());
        if lo < hi {
            local_code.push((lo, hi));
        }
    }
    // Fast path: no inline code on this line → defer to the plain builder so
    // the common case is byte-for-byte identical to before.
    if local_code.is_empty() {
        return line_spans(prefix, prefix_style, text, selected, base, selected_bg);
    }

    // Collect every boundary that can change the run style: line start/end, the
    // selection edges, and each code range's edges. Sort + dedupe, then walk
    // adjacent pairs emitting one span per uniform segment.
    let mut points: Vec<usize> = vec![0, text.len()];
    if let Some((lo, hi)) = selected {
        points.push(lo);
        points.push(hi);
    }
    for &(lo, hi) in &local_code {
        points.push(lo);
        points.push(hi);
    }
    points.sort_unstable();
    points.dedup();

    let is_code = |p: usize| local_code.iter().any(|&(lo, hi)| p >= lo && p < hi);
    let is_sel = |p: usize| matches!(selected, Some((lo, hi)) if p >= lo && p < hi);

    let mut i = 0;
    while i + 1 < points.len() {
        let seg_lo = points[i];
        let seg_hi = points[i + 1];
        i += 1;
        if seg_lo >= seg_hi {
            continue;
        }
        let code = is_code(seg_lo);
        let sel = is_sel(seg_lo);
        let style = match (code, sel) {
            (false, false) => base,
            (false, true) => base.bg(selected_bg),
            (true, false) => Style::default().fg(code_fg).bg(code_bg),
            (true, true) => Style::default().fg(code_fg).bg(selected_bg),
        };
        spans.push(Span::styled(text[seg_lo..seg_hi].to_string(), style));
    }
    Line::from(spans)
}

/// Build a rendered code line with a line-number gutter, a uniform `code_bg`
/// band that fills the full width, and character-level selection highlighting.
/// The optional left bar is retained for callers that want it; code blocks
/// themselves render borderless with only the gutter as ornament.
#[allow(clippy::too_many_arguments)]
pub(super) fn code_gutter_line(
    left_bar: Color,
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

    if left_bar != Color::Reset {
        spans.push(Span::styled("┃", Style::default().bg(code_bg).fg(left_bar)));
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
pub(super) struct WrappedLine {
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Wrap text into lines that fit within `max_width` display columns.
/// Returns each line along with the byte range it covers in the original text.
pub(super) fn wrap_text(text: &str, max_width: usize) -> Vec<WrappedLine> {
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
            // Only pop when we know a character will move down with the
            // wrap; `count() > 1` guarantees `pop()` yields `Some` while
            // leaving at least one character on the current line.
            let to_move = (move_previous && current_line.chars().count() > 1)
                .then(|| current_line.pop())
                .flatten();
            if let Some(moved) = to_move {
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

pub(super) fn prohibited_line_start(ch: char) -> bool {
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

pub(super) fn prohibited_line_end(ch: char) -> bool {
    matches!(
        ch,
        '（' | '【' | '《' | '〈' | '「' | '『' | '〔' | '“' | '‘' | '(' | '[' | '{'
    )
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`line_spans_rich`]. These cover the three things that
    //! matter for inline-code rendering: copy fidelity (the concatenated span
    //! content equals the source text), band application (the code run and
    //! only the code run carries `code_bg`), and selection interaction.

    use super::line_spans_rich;
    use neenee_tui::{Color, Style};

    /// Rebuild the rendered text from a line's spans, skipping the prefix.
    fn rendered_content(line: &neenee_tui::Line<'_>) -> String {
        line.spans
            .iter()
            .skip(1)
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn no_code_defers_to_plain_builder() {
        // A line with no code range must produce a single content span (the
        // fast path), identical to the plain `line_spans`.
        let line = line_spans_rich(
            "",
            Style::default(),
            "plain text",
            0,
            None,
            &[],
            Style::default().fg(Color::White),
            Color::Green,
            Color::Black,
            Color::Red,
        );
        // prefix (empty) + one content span
        assert_eq!(line.spans.len(), 2);
        assert_eq!(rendered_content(&line), "plain text");
    }

    #[test]
    fn code_run_is_painted_on_code_bg_and_round_trips() {
        // "use `foo` now" with the code range covering `` `foo` ``.
        let text = "use `foo` now";
        let code_range = (4, 9); // "`foo`"
        let line = line_spans_rich(
            "",
            Style::default(),
            text,
            0,
            None,
            &[code_range],
            Style::default().fg(Color::White),
            Color::Green,
            Color::Black,
            Color::Red,
        );
        assert_eq!(rendered_content(&line), text);

        // Three content spans: "use ", "`foo`", " now".
        let content: Vec<_> = line.spans.iter().skip(1).collect();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0].content, "use ");
        assert_eq!(content[1].content, "`foo`");
        assert_eq!(content[2].content, " now");
        // The code span carries the code background; the plain ones do not.
        assert_eq!(content[0].style.bg, Color::Reset);
        assert_eq!(content[1].style.bg, Color::Black);
        assert_eq!(content[2].style.bg, Color::Reset);
        assert_eq!(content[1].style.fg, Color::Green);
    }

    #[test]
    fn selection_overrides_background() {
        // Selecting the whole line paints every span with `selected_bg`,
        // including the code run.
        let text = "a `b` c";
        let line = line_spans_rich(
            "",
            Style::default(),
            text,
            0,
            Some((0, text.len())),
            &[(2, 5)],
            Style::default().fg(Color::White),
            Color::Green,
            Color::Black,
            Color::Red,
        );
        let content: Vec<_> = line.spans.iter().skip(1).collect();
        assert!(content.iter().all(|s| s.style.bg == Color::Red));
        assert_eq!(rendered_content(&line), text);
    }

    #[test]
    fn code_range_outside_line_is_ignored() {
        // `line_start_byte` positions this line at byte 100; a code range
        // living in [0,6) is entirely before it and must be ignored.
        let line = line_spans_rich(
            "",
            Style::default(),
            "no code here",
            100,
            None,
            &[(0, 6)],
            Style::default().fg(Color::White),
            Color::Green,
            Color::Black,
            Color::Red,
        );
        assert_eq!(line.spans.len(), 2); // fast path
        assert_eq!(rendered_content(&line), "no code here");
    }
}
