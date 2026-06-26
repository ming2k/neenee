//! Display primitives: [`Color`], [`Style`], [`Modifier`], and [`Cell`].
//!
//! The palette is RGB-first. neenee's curated near-black surface and accent
//! tokens are all `Color::Rgb`, so the diff/dim math only needs to handle the
//! RGB case; the named variants exist for interop and the terminal default
//! (`Color::Reset`). This mirrors the approach the old `primitives.rs`
//! `scale_color` took, lifted here as the canonical definition.
//!
//! A [`Cell`] is the unit a grid stores per (row, col). It carries one
//! grapheme cluster (which may be multi-codepoint, e.g. `e\u{0301}`) plus its
//! display width so the grid never has to re-measure. Wide (CJK) glyphs own a
//! trailing cell that is a [`Cell::wide_continuation`] — it carries the head's
//! background so a stale multiplexer grid can never ghost it (ADR-0038). This
//! is the fix ratatui cannot express because its `set_stringn` `reset()`s the
//! trailing cell to `Color::Reset`.

use std::fmt;

/// A terminal color.
///
/// `Reset` means "the terminal's default" and is intentionally distinct from
/// any RGB value: it is what blank padding and cleared cells resolve to when
/// the terminal (or multiplexer) paints them. The application paints a custom
/// near-black surface everywhere it *renders content*, so `Reset` only appears
/// in genuinely empty cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    #[default]
    Reset,
    Rgb(u8, u8, u8),
    Black,
    White,
}

impl Color {
    /// The RGB triple if this is an RGB color, else `None`.
    pub fn as_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            _ => None,
        }
    }

    /// Scale an RGB color's channels toward black by `factor`
    /// (0.0 = black, 1.0 = unchanged). Non-RGB colors pass through untouched
    /// so the dim is additive rather than lossy where they appear. This is
    /// the recess-backdrop dim math, lifted from the old `primitives.rs`.
    pub fn scale(self, factor: f32) -> Color {
        let f = factor.clamp(0.0, 1.0);
        match self {
            Color::Rgb(r, g, b) => Color::Rgb(
                (r as f32 * f).round() as u8,
                (g as f32 * f).round() as u8,
                (b as f32 * f).round() as u8,
            ),
            other => other,
        }
    }

    /// Relative luminance in `[0, 255]`, used to pick a contrast foreground
    /// for a colored fill (dark text on light backgrounds). Named colors map
    /// to their conventional approximate RGB so the recede/contrast heuristic
    /// still works for any stray named color.
    pub fn luminance(self) -> f32 {
        let (r, g, b) = match self {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Black => (0, 0, 0),
            Color::White => (255, 255, 255),
            Color::Reset => (0, 0, 0),
        };
        0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
    }

    /// Black on light fills, white on dark fills.
    pub fn contrast_fg(self) -> Color {
        if self.luminance() > 140.0 {
            Color::Black
        } else {
            Color::White
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Color::Reset => f.write_str("reset"),
            Color::Rgb(r, g, b) => write!(f, "#{r:02X}{g:02X}{b:02X}"),
            Color::Black => f.write_str("#000"),
            Color::White => f.write_str("#FFF"),
        }
    }
}

impl From<(u8, u8, u8)> for Color {
    fn from((r, g, b): (u8, u8, u8)) -> Self {
        Color::Rgb(r, g, b)
    }
}

bitflags::bitflags! {
    /// Text attributes. Matches the SGR (Select Graphic Rendition) attribute
    /// bits a terminal can render. Kept as `bitflags` so styles compose with
    /// `|` and a diff can test "which bits changed" with `^`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct Modifier: u16 {
        const BOLD      = 1 << 0;
        const DIM       = 1 << 1;
        const ITALIC    = 1 << 2;
        const UNDERLINE = 1 << 3;
        const REVERSE   = 1 << 4;
        const STRIKETHROUGH = 1 << 5;
    }
}

/// The full presentation of one cell: foreground/background color and
/// attributes. Two styles diff to "what SGR sequence turns the old into the
/// new", which is what the backend emits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub add: Modifier,
}

impl Style {
    /// Style with only a foreground color.
    pub fn fg(self, fg: Color) -> Self {
        Self { fg, ..self }
    }
    /// Style with only a background color.
    pub fn bg(self, bg: Color) -> Self {
        Self { bg, ..self }
    }
    /// Add attributes.
    pub fn with_add(self, add: Modifier) -> Self {
        Self {
            add: self.add | add,
            ..self
        }
    }

    /// The default "nothing rendered" style: default fg, default bg, no attrs.
    /// A cell styled this way reads as the terminal's default for both colors.
    pub const RESET: Style = Style {
        fg: Color::Reset,
        bg: Color::Reset,
        add: Modifier::empty(),
    };

    /// Convenience: a style that only sets the background (fg stays default).
    pub fn fill(bg: Color) -> Self {
        Style { bg, ..Style::RESET }
    }
}

/// A single grid cell.
///
/// `symbol` is the grapheme cluster to render (a `String` because clusters
/// may be multi-codepoint; compared by equality for diffing). `width` is its
/// display width in columns (0, 1, or 2), cached at write time so the diff
/// never re-measures. `style` is its presentation.
///
/// A wide glyph (width 2) occupies this cell as the head; the column to its
/// right is a [`Cell::wide_continuation`] carrying the same background, so the
/// glyph's background spillover is always correct even if a multiplexer
/// repaints that column on its own. This is the property ratatui's buffer
/// cannot provide (it `reset()`s the trailing cell to `Color::Reset`), and the
/// reason this engine exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub symbol: String,
    pub width: u8,
    pub style: Style,
}

impl Cell {
    /// A blank cell: one space column, default style. This is what an empty
    /// grid is filled with and what `clr_eol`/`clr_eos` conceptually write.
    pub fn blank() -> Self {
        Cell {
            symbol: " ".to_string(),
            width: 1,
            style: Style::RESET,
        }
    }

    /// A blank cell pre-styled with a background — used to fill a band with a
    /// surface color (the app-bg fill, a panel background).
    pub fn blank_styled(style: Style) -> Self {
        Cell {
            symbol: " ".to_string(),
            width: 1,
            style,
        }
    }

    /// The trailing cell of a width-2 glyph. It renders nothing of its own
    /// (the head's glyph spills into this column) but owns the head's
    /// background so the column can never ghost.
    pub fn wide_continuation(head_style: Style) -> Self {
        Cell {
            // Empty symbol: the backend skips emitting this cell directly;
            // its background is carried implicitly by the head glyph's SGR.
            // Kept as a single-space so a naive string dump still lines up.
            symbol: " ".to_string(),
            width: 0,
            style: Style {
                bg: head_style.bg,
                ..Style::RESET
            },
        }
    }

    /// A narrow (width-1) glyph with a style.
    pub fn narrow(symbol: impl Into<String>, style: Style) -> Self {
        Cell {
            symbol: symbol.into(),
            width: 1,
            style,
        }
    }

    /// Whether this cell is the trailing half of a wide glyph.
    pub fn is_wide_continuation(&self) -> bool {
        self.width == 0
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank()
    }
}
