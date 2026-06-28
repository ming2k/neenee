//! Grid diff: compare a dirty back grid against the front grid (what the
//! terminal currently shows) and emit a minimal stream of [`Draw`] commands.
//!
//! This is the nvim `grid_line` analog. It walks only the dirty rows and only
//! from each row's leftmost dirty column, grouping consecutive equal-style
//! cells into run-length packed [`Draw::Cells`] commands. Style changes start
//! a new run; the backend translates each run into one cursor-move + one SGR
//! set + one byte write.
//!
//! The diff is pure: it produces commands but does not touch crossterm. That
//! keeps it unit-testable (feed two grids, assert the commands) and lets the
//! backend own all I/O and capability negotiation (BCE, color depth).

use crate::Style;
use crate::grid::Grid;

/// One logical draw operation the backend should perform. The backend turns
/// these into escape codes. Keeping them logical (not raw bytes) means the
/// same diff drives a real terminal and a test capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Draw {
    /// Paint a run of cells starting at `(x, y)`. Each cell carries its symbol
    /// and the uniform style for the whole run. The run is contiguous and the
    /// cursor should be positioned at `(x, y)` before writing; the cells
    /// occupy `cells.len()` columns (wide continuations are emitted as their
    /// head glyph's implicit trailing column, so callers should skip
    /// continuation cells when counting).
    Cells {
        x: u16,
        y: u16,
        style: Style,
        /// `(symbol, width)` pairs. Wide-glyph continuation cells are omitted
        /// from this list (the head glyph paints both columns); the backend
        /// advances the cursor by each symbol's width.
        cells: Vec<(String, u8)>,
    },
    /// Clear from `(x, y)` to the end of that row with `style`. On BCE
    /// terminals the backend emits `clr_eol`; without BCE it paints `width`
    /// explicit styled spaces.
    ClearEol {
        x: u16,
        y: u16,
        style: Style,
        width: u16,
    },
}

/// A complete diff over a grid: the list of draw commands plus whether the
/// whole region should be considered repainted (used to drive cursor show/hide
/// and scroll bookkeeping).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawCmd {
    pub draws: Vec<Draw>,
    pub w: u16,
    pub h: u16,
}

/// Diff `back` (desired) against `front` (current terminal state), emitting
/// draw commands only for the dirty region. Reads `back`'s dirty bookkeeping
/// and does not modify either grid — promotion is a separate step
/// ([`promote`](crate::grid::Grid) happens after the backend applies the
/// commands).
///
/// `front` must be the same size as `back`; if not, the caller resized the
/// back grid and should resize the front to match before diffing (the resize
/// marks everything dirty, so this diff then repaints the whole grid).
pub fn diff(back: &Grid, front: &Grid) -> DrawCmd {
    let (w, h) = back.size();
    debug_assert_eq!(
        front.size(),
        (w, h),
        "front grid must match back grid size before diffing"
    );

    let mut draws = Vec::new();

    let Some((lo, hi)) = back.dirty_rows() else {
        return DrawCmd { draws, w, h };
    };

    for y in lo..=hi {
        let Some(start) = back.dirty_col_of(y) else {
            continue;
        };
        diff_row(&mut draws, back, front, y, start, w);
    }

    DrawCmd { draws, w, h }
}

/// Diff one row from `start_col` to the right edge, appending draw commands.
fn diff_row(draws: &mut Vec<Draw>, back: &Grid, front: &Grid, y: u16, start: u16, w: u16) {
    // Walk the row; whenever the back cell differs from the front cell, start
    // accumulating a run. Runs group cells that share a style; a style change
    // flushes the current run and starts a new one.
    let mut run_x = None;
    let mut run_style = Style::RESET;
    let mut run_cells: Vec<(String, u8)> = Vec::new();

    let mut x = start;
    while x < w {
        #[allow(clippy::unwrap_used)]
        // fallible only as the fallback branch of unwrap_or_else; never panics
        let back_cell = back.get(x, y).unwrap_or_else(|| front.get(x, y).unwrap());
        let front_cell = front.get(x, y).unwrap_or(back_cell);

        // Skip wide continuations in the output: their head (at x-1) paints
        // both columns. We still consume them so the cursor advances.
        if back_cell.is_wide_continuation() {
            x += 1;
            continue;
        }

        if back_cell == front_cell {
            // Cell unchanged: flush any open run, then skip.
            flush_run(draws, &mut run_x, &mut run_style, &mut run_cells, y);
            x += 1;
            continue;
        }

        if let Some((tail_style, tail_width)) = blank_tail(back, y, x, w) {
            flush_run(draws, &mut run_x, &mut run_style, &mut run_cells, y);
            draws.push(Draw::ClearEol {
                x,
                y,
                style: tail_style,
                width: tail_width,
            });
            break;
        }

        // Cell changed. If the style differs from the run's, flush first.
        if run_x.is_none() {
            run_x = Some(x);
            run_style = back_cell.style;
        } else if back_cell.style != run_style {
            flush_run(draws, &mut run_x, &mut run_style, &mut run_cells, y);
            run_x = Some(x);
            run_style = back_cell.style;
        }

        run_cells.push((back_cell.symbol.clone(), back_cell.width));
        x += if back_cell.width == 0 {
            1
        } else {
            back_cell.width as u16
        };
    }

    flush_run(draws, &mut run_x, &mut run_style, &mut run_cells, y);
}

/// Return the uniform style and width of a blank tail starting at `x`, if the
/// desired row from `x..w` is all width-1 spaces with the same style.
fn blank_tail(back: &Grid, y: u16, x: u16, w: u16) -> Option<(Style, u16)> {
    let first = back.get(x, y)?;
    if first.symbol != " " || first.width != 1 {
        return None;
    }
    let style = first.style;
    for col in x + 1..w {
        let cell = back.get(col, y)?;
        if cell.symbol != " " || cell.width != 1 || cell.style != style {
            return None;
        }
    }
    Some((style, w.saturating_sub(x)))
}

/// Emit a pending run as a `Draw::Cells` (if non-empty).
fn flush_run(
    draws: &mut Vec<Draw>,
    run_x: &mut Option<u16>,
    run_style: &mut Style,
    run_cells: &mut Vec<(String, u8)>,
    y: u16,
) {
    if let Some(x) = run_x.take()
        && !run_cells.is_empty()
    {
        draws.push(Draw::Cells {
            x,
            y,
            style: *run_style,
            cells: std::mem::take(run_cells),
        });
    }
    *run_style = Style::RESET;
}

/// Promote the back grid's dirty cells into the front grid, then clear the
/// back grid's dirty bookkeeping. Called by the frame loop *after* the backend
/// has applied the diff's commands — at that point the front grid faithfully
/// mirrors the terminal again.
pub fn promote(back: &mut Grid, front: &mut Grid) {
    let (w, _h) = back.size();
    if let Some((lo, hi)) = back.dirty_rows() {
        for y in lo..=hi {
            let Some(start) = back.dirty_col_of(y) else {
                continue;
            };
            for x in start..w {
                if let Some(cell) = back.get(x, y) {
                    let cell = cell.clone();
                    if let Some(dst) = front.cell_mut(x, y) {
                        *dst = cell;
                    }
                }
            }
        }
    }
    back.clear_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Color;
    use crate::cell::Cell;

    fn grid(text: &str, w: u16, h: u16) -> Grid {
        let mut g = Grid::new(w, h);
        g.put(0, 0, crate::grid::Fit::Clip, Style::default(), text);
        g.clear_dirty();
        g
    }

    #[test]
    fn identical_grids_emit_nothing() {
        let back = grid("abc", 4, 1);
        let mut front = grid("abc", 4, 1);
        let cmd = diff(&back, &front);
        assert!(cmd.draws.is_empty());
        promote(&mut Grid::new(4, 1), &mut front); // no-op smoke
    }

    #[test]
    fn single_cell_change_emits_one_run() {
        let mut back = grid("abc", 4, 1);
        // Change 'b' to 'B'.
        back.set(1, 0, Cell::narrow("B", Style::default()));
        let front = grid("abc", 4, 1);

        let cmd = diff(&back, &front);
        assert_eq!(cmd.draws.len(), 1);
        match &cmd.draws[0] {
            Draw::Cells { x, y, cells, .. } => {
                assert_eq!(*x, 1);
                assert_eq!(*y, 0);
                assert_eq!(cells, &vec![("B".to_string(), 1)]);
            }
            other => panic!("expected Cells, got {other:?}"),
        }
    }

    #[test]
    fn run_breaks_on_style_change() {
        let mut back = grid("abcd", 4, 1);
        // Restyle 'c' (col 2) with a different fg.
        back.set(
            2,
            0,
            Cell::narrow("c", Style::default().fg(Color::Rgb(1, 1, 1))),
        );
        // And change 'd' too with the same new style → same run as 'c'.
        back.set(
            3,
            0,
            Cell::narrow("D", Style::default().fg(Color::Rgb(1, 1, 1))),
        );
        let front = grid("abcd", 4, 1);

        let cmd = diff(&back, &front);
        // One run: cols 2..4, uniform style.
        assert_eq!(cmd.draws.len(), 1);
    }

    #[test]
    fn wide_glyph_head_emitted_continuation_skipped() {
        let mut back = grid("", 6, 1);
        back.put(0, 0, crate::grid::Fit::Clip, Style::default(), "😀a");
        let front = Grid::new(6, 1);

        let cmd = diff(&back, &front);
        // The continuation cell at col 1 is skipped; we get one run with the
        // wide head and 'a'.
        assert_eq!(cmd.draws.len(), 1);
        match &cmd.draws[0] {
            Draw::Cells { cells, .. } => {
                assert_eq!(cells[0], ("😀".to_string(), 2));
                assert_eq!(cells[1], ("a".to_string(), 1));
                assert_eq!(cells.len(), 2);
            }
            other => panic!("expected Cells, got {other:?}"),
        }
    }

    #[test]
    fn clean_rows_outside_dirty_range_are_skipped() {
        let mut back = grid("abc", 4, 3);
        // Dirty only row 2.
        back.clear_dirty();
        back.set(0, 2, Cell::narrow("Z", Style::default()));
        let front = grid("abc", 4, 3);

        let cmd = diff(&back, &front);
        assert!(
            cmd.draws
                .iter()
                .all(|d| matches!(d, Draw::Cells { y, .. } if *y == 2))
        );
    }

    #[test]
    fn promote_syncs_front_and_clears_dirty() {
        let mut back = grid("abc", 4, 1);
        back.set(1, 0, Cell::narrow("B", Style::default()));
        let mut front = grid("abc", 4, 1);

        promote(&mut back, &mut front);
        assert_eq!(front.get(1, 0).unwrap().symbol, "B");
        assert!(!back.is_dirty());
        // A second diff against the promoted front is now empty.
        assert!(diff(&back, &front).draws.is_empty());
    }

    #[test]
    fn wide_glyph_selection_toggle_diffs_head_only() {
        use crate::grid::Fit;
        let panel = Color::Rgb(18, 19, 19);
        let sel = Color::Rgb(38, 48, 44);
        let w = 6u16;
        // front: wide glyph UNselected + panel tail
        let mut front = Grid::new(w, 1);
        front.put(
            0,
            0,
            Fit::Clip,
            Style::default().bg(panel).fg(Color::White),
            "中",
        );
        for x in 2..w {
            front.set(x, 0, Cell::blank_styled(Style::default().bg(panel)));
        }
        front.clear_dirty();
        // back: wide glyph SELECTED + panel tail
        let mut back = Grid::new(w, 1);
        back.put(
            0,
            0,
            Fit::Clip,
            Style::default().bg(sel).fg(Color::White),
            "中",
        );
        for x in 2..w {
            back.set(x, 0, Cell::blank_styled(Style::default().bg(panel)));
        }
        let cmd = diff(&back, &front);
        // The wide head must be emitted with the SELECTED bg.
        assert!(cmd.draws.iter().any(|d| matches!(d,
                Draw::Cells { style, cells, .. }
                if style.bg == sel && cells.iter().any(|(s, _)| s == "中"))));
        // No ClearEol may clobber the wide glyph's columns (0..2).
        for d in &cmd.draws {
            if let Draw::ClearEol { x, .. } = d {
                assert!(*x >= 2, "ClearEol at x={x} would clobber the wide glyph");
            }
        }
    }
}
