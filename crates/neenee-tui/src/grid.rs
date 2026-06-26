//! The retained cell grid: the single source of truth for what the
//! application wants on screen, plus the front-grid mirror of what the
//! terminal currently shows.
//!
//! # Model (ADR-0038)
//!
//! This is vim's `ScreenGrid` model, not ratatui's immediate-mode buffer. A
//! [`Grid`] is a 2-D array of [`Cell`]s in row-major order, plus a per-line
//! **dirty column** (`dirty_col[row]`) recording the leftmost column that
//! changed on that row since the last flush, and a `dirty_row_lo/hi` pair
//! bounding the dirty row range. Writes mark dirt *at write time*, so the diff
//! never has to rescan the whole grid — it walks only rows in the dirty range
//! and only from each row's `dirty_col` onward.
//!
//! There are two grids in play:
//!
//! - the **back** grid (`Grid::default`): what the app wants.
//! - the **front** grid: what the terminal currently shows. After the backend
//!   applies a diff's [`Draw`] commands, the back grid's dirty cells are
//!   promoted into the front grid (see [`Grid::promote`]).
//!
//! The application never touches the front grid directly; it only writes the
//! back grid and lets the frame loop diff + promote.
//!
//! # Wide glyphs
//!
//! Writing a width-2 glyph at `(x, y)` also writes a wide-continuation cell at
//! `(x+1, y)` carrying the glyph's background, so the trailing column can
//! never ghost (ADR-0038). Writing past the right edge wraps or clips per the
//! [`put`] options; a glyph that would straddle the edge is never split — it
//! moves to the next row (wrap) or is dropped (clip).

use crate::cell::Cell;
use crate::text::{grapheme_width, graphemes};

/// A `(row, col)` position on the grid. `(0,0)` is the top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pos {
    pub x: u16,
    pub y: u16,
}

impl Pos {
    pub fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

/// How [`Grid::put`] handles a glyph that doesn't fit on the current line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Drop the glyph entirely if it would overflow the right edge.
    #[default]
    Clip,
    /// Move to column 0 of the next row before drawing. If there is no next
    /// row, the glyph is dropped.
    Wrap,
}

/// A retained cell grid with write-marks-dirty tracking.
///
/// Stored as a flat `Vec<Cell>` in row-major order. The dirty bookkeeping
/// (`dirty_col`, `dirty_row_lo`, `dirty_row_hi`) is updated on every mutating
/// call so [`Grid::diff`] only walks the region that actually changed.
pub struct Grid {
    pub width: u16,
    pub height: u16,
    /// The cell array, row-major. Public so the primitives layer (scrollbar,
    /// dim-recess) can index it directly the way it indexed ratatui's
    /// `Buffer::content`. Writes here bypass dirty tracking — callers that
    /// mutate in place must call [`Grid::mark`] for the touched content, or
    /// [`Grid::mark_all_dirty`] for a wholesale edit.
    pub content: Vec<Cell>,
    /// Alias for [`Grid::cells`] matching ratatui's `Buffer::content` naming,
    /// so migrated code that reads `buf.content[idx]` works without changes.
    /// This is a method returning a slice because a field alias would require
    /// duplicating the storage; callers should use `.cells` directly in new
    /// code.
    // Note: we can't have both `cells` field and `content` field. Callers
    // that used `buf.content` need to use `buf.cells` instead. The migration
    // script handles this rename.
    /// Leftmost changed column on each row since the last flush, or `None`
    /// when the row is clean. Mirrors vim's per-line `dirty_col`.
    pub(crate) dirty_col: Vec<Option<u16>>,
    /// Inclusive bounds of the dirty row range, or `None` when nothing is
    /// dirty. Lets the diff skip entire clean regions.
    pub(crate) dirty_row_lo: Option<u16>,
    pub(crate) dirty_row_hi: Option<u16>,
}

impl Grid {
    /// Create a blank grid of the given size, filled with default cells and
    /// fully clean (nothing dirty). This is the back-grid starting state.
    pub fn new(width: u16, height: u16) -> Self {
        let content = vec![Cell::blank(); (width as usize) * (height as usize)];
        Self {
            width,
            height,
            content,
            dirty_col: vec![None; height as usize],
            dirty_row_lo: None,
            dirty_row_hi: None,
        }
    }

    /// Resize the grid. Existing content in the overlapping top-left region is
    /// preserved; newly exposed cells are blank and marked dirty so the next
    /// diff repaints them. This is the resize path: the application resizes
    /// the back grid, then the next frame's diff + promote updates the front.
    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        let mut next = vec![Cell::blank(); (width as usize) * (height as usize)];
        let copy_w = self.width.min(width);
        let copy_h = self.height.min(height);
        for y in 0..copy_h {
            let src = y as usize * self.width as usize;
            let dst = y as usize * width as usize;
            next[dst..dst + copy_w as usize]
                .clone_from_slice(&self.content[src..src + copy_w as usize]);
        }
        self.content = next;
        self.width = width;
        self.height = height;
        self.dirty_col = vec![None; height as usize];
        // Mark every row dirty: the front grid is the old size and must be
        // fully reconciled, including blanked cells.
        for y in 0..height {
            self.dirty_col[y as usize] = Some(0);
        }
        self.dirty_row_lo = (height > 0).then_some(0);
        self.dirty_row_hi = (height > 0).then_some(height.saturating_sub(1));
    }

    /// The grid dimensions.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Immutably read a cell. Out-of-bounds reads return `None`.
    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.content.get(self.index_of(x, y))
    }

    /// Mutably access a cell without marking it dirty. Use when the caller
    /// intends to read-modify-write and will call a marking helper, or for
    /// internal diff machinery.
    pub(crate) fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = self.index_of(x, y);
        self.content.get_mut(idx)
    }

    /// Write a cell at `(x, y)`, marking that row dirty from column `x`.
    /// Out-of-bounds writes are silently dropped. The caller is responsible
    /// for wide-glyph continuation handling (see [`Self::put`]).
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = self.index_of(x, y);
        self.content[idx] = cell;
        self.mark(x, y);
    }

    /// Write a string at `(x, y)` with a uniform style, honoring grapheme
    /// widths: a width-2 glyph also fills its trailing column with a
    /// wide-continuation cell. Returns the position after the last drawn
    /// column. Honors [`Fit`] for overflow.
    ///
    /// Embedded newlines move to column 0 of the next row (regardless of
    /// `Fit`), mirroring how text is laid out line by line.
    pub fn put(&mut self, x: u16, y: u16, fit: Fit, style: crate::Style, text: &str) -> Pos {
        let mut cx = x;
        let mut cy = y;

        for piece in graphemes(text) {
            let grapheme = piece.text;
            if grapheme == "\n" {
                cy = match cy.checked_add(1) {
                    Some(next) if next < self.height => next,
                    _ => break,
                };
                cx = 0;
                continue;
            }
            let w = grapheme_width(grapheme);
            // Overflow handling.
            if cx + w as u16 > self.width {
                match fit {
                    Fit::Wrap => {
                        cy = match cy.checked_add(1) {
                            Some(next) if next < self.height => next,
                            _ => break,
                        };
                        cx = 0;
                    }
                    Fit::Clip => {
                        // Drop the rest if it can't fit even on a fresh line,
                        // else just skip this glyph.
                        if w as u16 > self.width {
                            continue;
                        }
                        continue;
                    }
                }
            }
            // Place the head cell. A Reset background means "transparent" for
            // text writes: preserve the surface already painted under this
            // cell instead of punching through to the terminal default.
            let mut cell_style = style;
            if cell_style.bg == crate::Color::Reset {
                if let Some(existing) = self.get(cx, cy) {
                    cell_style.bg = existing.bg;
                }
            }
            let head = Cell {
                symbol: grapheme.to_string(),
                width: w,
                fg: cell_style.fg,
                bg: cell_style.bg,
                style: cell_style,
            };
            let head_idx = self.index_of(cx, cy);
            self.content[head_idx] = head;
            self.mark(cx, cy);
            // Place the trailing continuation cell for a wide glyph, carrying
            // the glyph's background so the column can never ghost.
            if w >= 2 && cx + 1 < self.width {
                let trail_idx = self.index_of(cx + 1, cy);
                self.content[trail_idx] = Cell::wide_continuation(cell_style);
                self.mark(cx + 1, cy);
            }
            cx += w as u16;
        }

        Pos { x: cx, y: cy }
    }

    /// Fill a rectangular region with blank cells of the given style (a solid
    /// colored band). Used for the app-background fill and panel backgrounds.
    pub fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, style: crate::Style) {
        let blank = Cell::blank_styled(style);
        for row in y..y.saturating_add(h).min(self.height) {
            for col in x..x.saturating_add(w).min(self.width) {
                let idx = self.index_of(col, row);
                self.content[idx] = blank.clone();
            }
            self.mark(x, row);
        }
    }

    /// Clear a single row to blank cells of the given style, from column `x`
    /// to the right edge.
    pub fn clear_row(&mut self, y: u16, x: u16, style: crate::Style) {
        if y >= self.height {
            return;
        }
        let blank = Cell::blank_styled(style);
        let start = self.index_of(x.min(self.width.saturating_sub(1)), y);
        let end = self.index_of(self.width, y);
        for cell in &mut self.content[start..end] {
            *cell = blank.clone();
        }
        self.mark(x, y);
    }

    /// Mark every cell dirty. Used after a wholesale replacement of contents
    /// (e.g. restoring from a session) so the next diff repaints everything.
    pub fn mark_all_dirty(&mut self) {
        for y in 0..self.height {
            self.dirty_col[y as usize] = Some(0);
        }
        if self.height > 0 {
            self.dirty_row_lo = Some(0);
            self.dirty_row_hi = Some(self.height - 1);
        }
    }

    /// Clear all dirty bookkeeping. Called after a diff is promoted into the
    /// front grid: the back grid now matches the terminal, so nothing is dirty.
    pub fn clear_dirty(&mut self) {
        for slot in &mut self.dirty_col {
            *slot = None;
        }
        self.dirty_row_lo = None;
        self.dirty_row_hi = None;
    }

    /// Whether any row is currently dirty.
    pub fn is_dirty(&self) -> bool {
        self.dirty_row_lo.is_some()
    }

    /// The inclusive dirty row range, or `None` if clean.
    pub fn dirty_rows(&self) -> Option<(u16, u16)> {
        match (self.dirty_row_lo, self.dirty_row_hi) {
            (Some(lo), Some(hi)) => Some((lo, hi)),
            _ => None,
        }
    }

    /// Leftmost dirty column on a row, or `None` if the row is clean.
    pub fn dirty_col_of(&self, y: u16) -> Option<u16> {
        self.dirty_col.get(y as usize).copied().flatten()
    }

    /// Row-major index for `(x, y)`. Caller guarantees in-bounds. Does not
    /// borrow `self` so it can be used within a mutable borrow of `cells`.
    #[inline]
    pub fn index_of(&self, x: u16, y: u16) -> usize {
        y as usize * self.width as usize + x as usize
    }

    /// The grid's bounding rect (origin 0,0). Convenience for renderers that
    /// ask `buf.area`.
    pub fn area(&self) -> crate::layout::Rect {
        crate::layout::Rect::new(0, 0, self.width, self.height)
    }

    /// Record that column `x` on row `y` changed, expanding the row's dirty
    /// window leftward and extending the dirty row range. Public so callers
    /// that mutate [`Grid::cells`] in place (scrollbar, dim) can keep the
    /// dirty tracking honest.
    #[inline]
    pub fn mark(&mut self, x: u16, y: u16) {
        let slot = &mut self.dirty_col[y as usize];
        match *slot {
            Some(lo) => *slot = Some(lo.min(x)),
            None => *slot = Some(x),
        }
        self.dirty_row_lo = Some(self.dirty_row_lo.map(|lo| lo.min(y)).unwrap_or(y));
        self.dirty_row_hi = Some(self.dirty_row_hi.map(|hi| hi.max(y)).unwrap_or(y));
    }
}

impl std::ops::Index<(u16, u16)> for Grid {
    type Output = Cell;
    fn index(&self, (x, y): (u16, u16)) -> &Cell {
        &self.content[y as usize * self.width as usize + x as usize]
    }
}

impl std::ops::IndexMut<(u16, u16)> for Grid {
    fn index_mut(&mut self, (x, y): (u16, u16)) -> &mut Cell {
        let idx = y as usize * self.width as usize + x as usize;
        &mut self.content[idx]
    }
}

impl std::ops::Index<usize> for Grid {
    type Output = Cell;
    fn index(&self, idx: usize) -> &Cell {
        &self.content[idx]
    }
}

impl std::ops::IndexMut<usize> for Grid {
    fn index_mut(&mut self, idx: usize) -> &mut Cell {
        &mut self.content[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Style;

    #[test]
    fn new_grid_is_clean() {
        let g = Grid::new(4, 3);
        assert!(!g.is_dirty());
        assert_eq!(g.dirty_rows(), None);
    }

    #[test]
    fn set_marks_only_that_row_from_column() {
        let mut g = Grid::new(4, 3);
        g.set(2, 1, Cell::narrow("x", Style::default()));
        assert_eq!(g.dirty_rows(), Some((1, 1)));
        assert_eq!(g.dirty_col_of(0), None);
        assert_eq!(g.dirty_col_of(1), Some(2));
        assert_eq!(g.dirty_col_of(2), None);
    }

    #[test]
    fn put_writes_wide_glyph_with_continuation() {
        let mut g = Grid::new(6, 1);
        g.put(0, 0, Fit::Clip, Style::default(), "😀a");
        assert_eq!(g.get(0, 0).unwrap().symbol, "😀");
        assert_eq!(g.get(0, 0).unwrap().width, 2);
        // Trailing continuation cell: width 0, same (default) bg.
        assert!(g.get(1, 0).unwrap().is_wide_continuation());
        assert_eq!(g.get(2, 0).unwrap().symbol, "a");
        // Whole row dirty from col 0.
        assert_eq!(g.dirty_col_of(0), Some(0));
    }

    #[test]
    fn put_wraps_on_overflow() {
        let mut g = Grid::new(3, 2);
        let end = g.put(2, 0, Fit::Wrap, Style::default(), "ab");
        // 'a' fits at col 2, 'b' wraps to row 1 col 0.
        assert_eq!(g.get(2, 0).unwrap().symbol, "a");
        assert_eq!(g.get(0, 1).unwrap().symbol, "b");
        assert_eq!(end, Pos { x: 1, y: 1 });
    }

    #[test]
    fn put_newline_moves_to_next_row() {
        let mut g = Grid::new(4, 2);
        g.put(0, 0, Fit::Clip, Style::default(), "ab\ncd");
        assert_eq!(g.get(0, 0).unwrap().symbol, "a");
        assert_eq!(g.get(1, 0).unwrap().symbol, "b");
        assert_eq!(g.get(0, 1).unwrap().symbol, "c");
        assert_eq!(g.get(1, 1).unwrap().symbol, "d");
    }

    #[test]
    fn clear_dirty_resets_bookkeeping() {
        let mut g = Grid::new(4, 3);
        g.set(1, 1, Cell::narrow("x", Style::default()));
        assert!(g.is_dirty());
        g.clear_dirty();
        assert!(!g.is_dirty());
        assert_eq!(g.dirty_col_of(1), None);
    }

    #[test]
    fn resize_preserves_overlap_and_marks_all() {
        let mut g = Grid::new(4, 2);
        g.put(0, 0, Fit::Clip, Style::default(), "abcd");
        g.clear_dirty();
        g.resize(6, 3);
        // Original content preserved.
        assert_eq!(g.get(0, 0).unwrap().symbol, "a");
        assert_eq!(g.get(3, 0).unwrap().symbol, "d");
        // New cells blank.
        assert_eq!(g.get(4, 0).unwrap().symbol, " ");
        // Everything dirty after resize.
        assert_eq!(g.dirty_rows(), Some((0, 2)));
        assert_eq!(g.dirty_col_of(0), Some(0));
        assert_eq!(g.dirty_col_of(2), Some(0));
    }
}
