//! Widgets: `Span`, `Line`, `Paragraph`, `Block`, `Clear`.
//!
//! These mirror ratatui's text/widget API so migrated widget code only needs
//! an import-path swap. The key difference: rendering writes into the
//! engine's [`Grid`] (via [`Frame::render_widget`]) instead of ratatui's
//! `Buffer`, so every paint goes through the write-marks-dirty path and the
//! wide-glyph trailing column is owned by the writer (ADR-0038).

use crate::layout::Rect;
use crate::text::wrap;
use crate::{Cell, Color, Style};

/// A styled string fragment.
#[derive(Debug, Clone)]
pub struct Span<'a> {
    pub content: std::borrow::Cow<'a, str>,
    pub style: Style,
}

impl<'a> Span<'a> {
    pub fn raw(content: impl Into<std::borrow::Cow<'a, str>>) -> Self {
        Self {
            content: content.into(),
            style: Style::RESET,
        }
    }
    pub fn styled(content: impl Into<std::borrow::Cow<'a, str>>, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }

    /// Display width of this span's content.
    pub fn width(&self) -> usize {
        crate::text::str_idth(&self.content)
    }
}

impl<'a> From<Vec<Span<'a>>> for Line<'a> {
    fn from(spans: Vec<Span<'a>>) -> Self {
        Self {
            spans,
            ..Default::default()
        }
    }
}
impl<'a> From<Span<'a>> for Line<'a> {
    fn from(span: Span<'a>) -> Self {
        Self {
            spans: vec![span],
            ..Default::default()
        }
    }
}
impl<'a> From<&'a str> for Line<'a> {
    fn from(s: &'a str) -> Self {
        Line::raw(s)
    }
}
impl<'a> From<&'a str> for Span<'a> {
    fn from(s: &'a str) -> Self {
        Span::raw(s)
    }
}
impl<'a> From<String> for Span<'a> {
    fn from(s: String) -> Self {
        Span::raw(s)
    }
}

/// A line of spans, with an optional overall style and alignment.
#[derive(Debug, Clone, Default)]
pub struct Line<'a> {
    pub spans: Vec<Span<'a>>,
    pub style: Style,
    pub alignment: Alignment,
}

impl<'a> Line<'a> {
    pub fn raw(content: impl Into<std::borrow::Cow<'a, str>>) -> Self {
        Self {
            spans: vec![Span::raw(content)],
            ..Default::default()
        }
    }
    /// Construct from a single span (any type that can become a `Span`).
    /// Note: this is `Line::from_span` not `Line::from` to avoid shadowing
    /// the `From<Vec<Span>>` trait impl.
    pub fn from_span<T: Into<Span<'a>>>(span: T) -> Self {
        Self {
            spans: vec![span.into()],
            ..Default::default()
        }
    }
    pub fn from_spans(spans: Vec<Span<'a>>) -> Self {
        Self {
            spans,
            ..Default::default()
        }
    }
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
    pub fn alignment(mut self, a: Alignment) -> Self {
        self.alignment = a;
        self
    }
    /// The total display width of this line (sum of span widths).
    pub fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|s| crate::text::str_idth(&s.content))
            .sum()
    }
}

/// Horizontal alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    #[default]
    Left,
    Center,
    Right,
}

/// A block (bordered panel). Supports a background fill and optional left
/// thick border bar (the only border styles neenee uses).
#[derive(Debug, Clone, Default)]
pub struct Block<'a> {
    pub style: Style,
    pub borders: Borders,
    pub border_type: BorderType,
    pub border_style: Style,
    pub title: Option<Line<'a>>,
}

/// Border sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Borders(pub u16);

impl Borders {
    pub const NONE: Borders = Borders(0);
    pub const LEFT: Borders = Borders(1);
    pub const RIGHT: Borders = Borders(2);
    pub const TOP: Borders = Borders(4);
    pub const BOTTOM: Borders = Borders(8);
    pub const fn union(self, other: Borders) -> Borders {
        Borders(self.0 | other.0)
    }
}

impl std::ops::BitOr for Borders {
    type Output = Borders;
    fn bitor(self, rhs: Borders) -> Borders {
        Borders(self.0 | rhs.0)
    }
}

/// Border rendering style. Only `Thick` (a solid `┃` bar) is used in prod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderType {
    #[default]
    Plain,
    Thick,
}

impl<'a> Block<'a> {
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
    pub fn borders(mut self, b: Borders) -> Self {
        self.borders = b;
        self
    }
    pub fn border_type(mut self, t: BorderType) -> Self {
        self.border_type = t;
        self
    }
    pub fn border_style(mut self, s: Style) -> Self {
        self.border_style = s;
        self
    }

    /// Render the block's background and borders into `grid`. Returns the
    /// inner rect (area minus borders/padding).
    pub fn render(&self, area: Rect, grid: &mut crate::Grid) {
        // Background fill.
        grid.fill_rect(area.x, area.y, area.width, area.height, self.style);
        // Left thick border. The bar uses the block bg so its background
        // spillover matches the panel, and the border_style fg.
        if self.borders.0 & Borders::LEFT.0 != 0 && area.width > 0 {
            let bar_style = Style {
                fg: self.border_style.fg,
                bg: self.style.bg,
                add: self.border_style.add,
            };
            for y in area.y..area.y + area.height {
                grid.set(area.x, y, Cell::narrow("┃", bar_style));
            }
        }
        // Right thick border.
        if self.borders.0 & Borders::RIGHT.0 != 0 && area.width > 1 {
            let bar_style = Style {
                fg: self.border_style.fg,
                bg: self.style.bg,
                add: self.border_style.add,
            };
            let rx = area.x + area.width - 1;
            for y in area.y..area.y + area.height {
                grid.set(rx, y, Cell::narrow("┃", bar_style));
            }
        }
    }
}

/// A clear operation: reset the target area to blank cells.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clear;

impl Clear {
    pub fn render(self, area: Rect, grid: &mut crate::Grid) {
        grid.fill_rect(area.x, area.y, area.width, area.height, Style::RESET);
    }
}

/// Word-wrapping configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wrap {
    pub trim: bool,
}

/// A paragraph: one or more lines, with optional scroll, wrap, alignment,
/// and an enclosing block.
#[derive(Debug, Clone, Default)]
pub struct Paragraph<'a> {
    pub lines: Vec<Line<'a>>,
    pub scroll: (u16, u16),
    pub wrap: Option<Wrap>,
    pub alignment: Alignment,
    pub block: Option<Block<'a>>,
    pub style: Style,
}

impl<'a> Paragraph<'a> {
    pub fn new<T: Into<ParagraphLines<'a>>>(content: T) -> Self {
        let lines = content.into().0;
        Self {
            lines,
            ..Default::default()
        }
    }
    pub fn scroll(mut self, row: u16, col: u16) -> Self {
        self.scroll = (row, col);
        self
    }
    pub fn wrap(mut self, w: Wrap) -> Self {
        self.wrap = Some(w);
        self
    }
    pub fn alignment(mut self, a: Alignment) -> Self {
        self.alignment = a;
        self
    }
    pub fn block(mut self, b: Block<'a>) -> Self {
        self.block = Some(b);
        self
    }
    pub fn style(mut self, s: Style) -> Self {
        self.style = s;
        self
    }

    /// Render the paragraph into `grid` within `area`. Handles block chrome,
    /// scroll offset, line wrapping, and alignment.
    pub fn render(&self, area: Rect, grid: &mut crate::Grid) {
        let inner = if let Some(b) = &self.block {
            b.render(area, grid);
            // No additional padding beyond borders (matches neenee usage).
            let mut ix = area.x;
            let mut iw = area.width;
            if b.borders.0 & Borders::LEFT.0 != 0 {
                ix += 1;
                iw = iw.saturating_sub(1);
            }
            if b.borders.0 & Borders::RIGHT.0 != 0 {
                iw = iw.saturating_sub(1);
            }
            Rect::new(ix, area.y, iw, area.height)
        } else {
            area
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let max_width = inner.width as usize;
        let row_offset = self.scroll.0 as usize;
        let mut y = inner.y;
        let bottom = inner.y + inner.height;
        let mut emitted = 0usize;

        for line in &self.lines {
            // Combine line.style with paragraph style (line wins on conflict).
            let base = merge_style(self.style, line.style);
            // Wrap this line into display rows.
            let wrapped = if self.wrap.is_some() {
                wrap_line(line, max_width)
            } else {
                vec![line.clone()]
            };
            for wl in &wrapped {
                if emitted < row_offset {
                    emitted += 1;
                    continue;
                }
                if y >= bottom {
                    return;
                }
                // Horizontal alignment within the inner width.
                let lw = line_display_width(wl);
                let x = match self.alignment {
                    Alignment::Left => inner.x + self.scroll.1,
                    Alignment::Center => inner.x + (inner.width.saturating_sub(lw as u16)) / 2,
                    Alignment::Right => inner.x + inner.width.saturating_sub(lw as u16),
                };
                // Clip each span to the rect's right edge. `grid.put` only
                // clips at the *terminal* edge, so without this a non-wrapped
                // line longer than `inner.width` (e.g. a modal footer hint)
                // would spill past the panel into the backdrop. Wrapped lines
                // already fit within `max_width`, so this is a no-op for them.
                let right = inner.x + inner.width;
                let mut cx = x;
                for span in &wl.spans {
                    if cx >= right {
                        break;
                    }
                    let s = merge_style(base, span.style);
                    let avail = (right - cx) as usize;
                    let content = clip_to_cols(&span.content, avail);
                    let end = grid.put(cx, y, crate::grid::Fit::Clip, s, content);
                    cx = end.x;
                }
                y += 1;
                emitted += 1;
            }
        }
    }
}

/// Helper for `Paragraph::new` accepting a single line or a vec.
pub struct ParagraphLines<'a>(pub Vec<Line<'a>>);

impl<'a> From<Line<'a>> for ParagraphLines<'a> {
    fn from(l: Line<'a>) -> Self {
        ParagraphLines(vec![l])
    }
}
impl<'a> From<&'a str> for ParagraphLines<'a> {
    fn from(s: &'a str) -> Self {
        // Split on newlines into separate lines.
        let lines = s.split('\n').map(Line::raw).collect();
        ParagraphLines(lines)
    }
}
impl<'a> From<String> for ParagraphLines<'a> {
    fn from(s: String) -> Self {
        let lines = s.split('\n').map(|l| Line::raw(l.to_string())).collect();
        ParagraphLines(lines)
    }
}
impl<'a> From<Vec<Line<'a>>> for ParagraphLines<'a> {
    fn from(v: Vec<Line<'a>>) -> Self {
        ParagraphLines(v)
    }
}

fn merge_style(base: Style, over: Style) -> Style {
    Style {
        fg: if over.fg != Color::Reset {
            over.fg
        } else {
            base.fg
        },
        bg: if over.bg != Color::Reset {
            over.bg
        } else {
            base.bg
        },
        add: base.add | over.add,
    }
}

fn line_display_width(l: &Line<'_>) -> usize {
    l.spans
        .iter()
        .map(|s| crate::text::str_len(&s.content))
        .sum()
}

/// The longest whole-grapheme prefix of `s` that fits within `max_cols`
/// display columns. Returns a borrowed slice (the full string when it already
/// fits), so the common in-bounds case allocates nothing. A wide glyph that
/// would straddle the boundary is dropped rather than half-drawn.
fn clip_to_cols(s: &str, max_cols: usize) -> &str {
    let mut used = 0usize;
    let mut bytes = 0usize;
    for piece in crate::text::graphemes(s) {
        let w = piece.width as usize;
        if used + w > max_cols {
            break;
        }
        used += w;
        bytes += piece.text.len();
    }
    &s[..bytes]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Grid;

    #[test]
    fn clip_to_cols_keeps_whole_graphemes() {
        assert_eq!(clip_to_cols("hello", 3), "hel");
        assert_eq!(clip_to_cols("hello", 99), "hello");
        assert_eq!(clip_to_cols("hello", 0), "");
        // A wide (CJK) glyph that would straddle the boundary is dropped, not
        // split in half.
        assert_eq!(clip_to_cols("世界", 1), "");
        assert_eq!(clip_to_cols("世界", 2), "世");
    }

    #[test]
    fn unwrapped_paragraph_clips_to_rect_not_terminal() {
        // A single-line (non-wrapped) Paragraph longer than its rect must stop
        // at the rect's right edge — never spill into the cells beyond it. This
        // is the guard against modal header/footer hints overflowing the panel.
        let mut grid = Grid::new(40, 1);
        // Sentinel content past the rect so we can detect a spill.
        grid.fill_rect(0, 0, 40, 1, Style::default());
        for x in 10..40 {
            grid.put(x, 0, crate::grid::Fit::Clip, Style::default(), "#");
        }
        let para = Paragraph::new("xxxxxxxxxxxxxxxxxxxxxxxxxxxx"); // 28 x's
        // Render into a 10-wide rect starting at column 0.
        para.render(Rect::new(0, 0, 10, 1), &mut grid);
        // Columns 0..10 are the x's; column 10 onward must keep the sentinel.
        for x in 0..10 {
            assert_eq!(grid.get(x, 0).unwrap().symbol(), "x", "col {x} in-rect");
        }
        assert_eq!(
            grid.get(10, 0).unwrap().symbol(),
            "#",
            "the cell just past the rect must not be overwritten"
        );
    }
}

fn wrap_line<'a>(line: &Line<'a>, max_width: usize) -> Vec<Line<'a>> {
    // Flatten the line's spans into one string, wrap it, then re-split into
    // spans by walking the wrapped byte ranges. This preserves per-span styles
    // across wrap boundaries.
    let mut flat = String::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new(); // (byte_start, byte_end) per span in flat
    for span in &line.spans {
        let start = flat.len();
        flat.push_str(&span.content);
        ranges.push((start, flat.len()));
    }
    let lines = wrap(&flat, max_width);
    let mut out = Vec::with_capacity(lines.len());
    for (i, wl) in lines.iter().enumerate() {
        let mut spans: Vec<Span<'a>> = Vec::new();
        let lo = wl.start_byte;
        let hi = wl.end_byte;
        for (j, span) in line.spans.iter().enumerate() {
            let (s, e) = ranges[j];
            // Intersect [lo,hi) with [s,e).
            let a = lo.max(s);
            let b = hi.min(e);
            if a < b {
                let rel = a - s;
                let len = b - a;
                if let Some(sub) = span.content.get(rel..rel + len) {
                    spans.push(Span::styled(sub.to_string(), span.style));
                }
            }
        }
        out.push(Line {
            spans,
            style: line.style,
            alignment: line.alignment,
        });
        let _ = i;
    }
    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}
