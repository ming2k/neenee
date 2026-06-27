//! Frame and Terminal: the draw-closure entry point the app calls each tick.
//!
//! `Frame` is the per-draw handle the application renders into. It borrows the
//! back [`Grid`] mutably, exposes ratatui-shaped methods (`area`,
//! `buffer_mut`, `render_widget`, `set_cursor_position`), and tracks the
//! desired terminal cursor position so the loop can emit a single move after
//! the closure returns.
//!
//! `Terminal` owns the [`Backend`], the back grid, and the front grid. Its
//! [`Terminal::draw`] runs the app closure against a fresh `Frame`, then
//! diffs the back grid against the front grid, hands the [`DrawCmd`] to the
//! backend, and promotes dirty cells into the front grid. Idle frames (no
//! dirty cells) emit nothing.

use std::io;

use crate::backend::Backend;
use crate::diff::{self, DrawCmd};
use crate::grid::{Fit, Grid};
use crate::layout::Rect;
use crate::widgets::Paragraph;

/// The terminal cursor mode the frame loop should end with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CursorState {
    #[default]
    Hidden,
    Visible(u16, u16),
}

/// A per-draw frame handle. Borrows the back grid for the duration of the
/// draw closure.
pub struct Frame<'a> {
    grid: &'a mut Grid,
    area_rect: Rect,
    cursor: CursorState,
}

impl<'a> Frame<'a> {
    /// Construct a frame over a grid. Public so that integration tests can
    /// drive the widget API directly.
    pub fn new(grid: &'a mut Grid) -> Self {
        let (w, h) = grid.size();
        Self {
            grid,
            area_rect: Rect::new(0, 0, w, h),
            cursor: CursorState::default(),
        }
    }

    /// Full-terminal area.
    pub fn area(&self) -> Rect {
        self.area_rect
    }

    /// Mutable access to the underlying grid, for in-place cell mutation
    /// (the dim-recess effect and the hand-rolled scrollbar).
    pub fn buffer_mut(&mut self) -> &mut Grid {
        self.grid
    }

    /// Render a widget into `area`. Only the three widget kinds neenee uses
    /// are supported (`Paragraph`, `Block`, `Clear`).
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.grid);
    }

    /// Set the terminal cursor position for after this frame's flush. The last
    /// call wins.
    pub fn set_cursor_position<P: Into<(u16, u16)>>(&mut self, pos: P) {
        let (x, y) = pos.into();
        self.cursor = CursorState::Visible(x, y);
    }

    /// Write a styled string directly into the grid (convenience for callers
    /// that don't want to build a `Paragraph`).
    pub fn put(&mut self, x: u16, y: u16, style: crate::Style, text: &str) {
        self.grid.put(x, y, Fit::Clip, style, text);
    }

    pub(crate) fn take_cursor(&mut self) -> CursorState {
        std::mem::take(&mut self.cursor)
    }
}

/// A widget that can render itself into a grid. Implemented for `Paragraph`,
/// `Block`, `Clear`, and `(Rect,)` passthrough.
pub trait Widget {
    fn render(self, area: Rect, grid: &mut Grid);
}

impl Widget for Paragraph<'_> {
    fn render(self, area: Rect, grid: &mut Grid) {
        Paragraph::render(&self, area, grid);
    }
}
impl Widget for crate::widgets::Block<'_> {
    fn render(self, area: Rect, grid: &mut Grid) {
        crate::widgets::Block::render(&self, area, grid);
    }
}
impl Widget for crate::widgets::Clear {
    fn render(self, area: Rect, grid: &mut Grid) {
        crate::widgets::Clear::render(self, area, grid);
    }
}

/// Owns the backend and the back/front grids. The application holds one of
/// these for the lifetime of the TUI.
pub struct Terminal<W: io::Write> {
    backend: Backend<W>,
    back: Grid,
    front: Grid,
    cursor: CursorState,
}

impl<W: io::Write> Terminal<W> {
    pub fn new(backend: Backend<W>) -> Self {
        // Size the grids to whatever the backend reports via crossterm.
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        let back = Grid::new(size.0, size.1);
        let front = Grid::new(size.0, size.1);
        Self {
            backend,
            back,
            front,
            cursor: CursorState::Hidden,
        }
    }

    /// Resize the back and front grids to the current terminal size.
    pub fn resize_to(&mut self, width: u16, height: u16) {
        self.back.resize(width, height);
        self.front = Grid::new(width, height);
        self.back.mark_all_dirty();
        // Best-effort: a resize must reconcile our SGR tracker with the real
        // terminal. If the write fails we still proceed — the next frame's
        // full repaint is the fallback and worst case is a transient style
        // glitch, not a hang.
        let _ = self.backend.invalidate();
        use crossterm::QueueableCommand;
        let _ = self.backend.writer().queue(crossterm::terminal::Clear(
            crossterm::terminal::ClearType::All,
        ));
    }

    /// Run the app's draw closure against a fresh frame, then diff → render
    /// → promote. Returns `Ok(())` on success.
    pub fn draw<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        // Sync grid size with terminal.
        if let Ok((w, h)) = crossterm::terminal::size() {
            if self.back.size() != (w, h) {
                self.back.resize(w, h);
                self.front = Grid::new(w, h);
                self.back.mark_all_dirty();
                // Reconcile the SGR tracker with the real terminal on resize.
                // `invalidate` emits a real `\x1b[0m`; a failure here only
                // risks a transient style glitch on the next frame, so we
                // swallow it rather than aborting the draw.
                let _ = self.backend.invalidate();
                use crossterm::QueueableCommand;
                let _ = self.backend.writer().queue(crossterm::terminal::Clear(
                    crossterm::terminal::ClearType::All,
                ));
            }
        }
        {
            let mut frame = Frame::new(&mut self.back);
            render(&mut frame);
            self.cursor = frame.take_cursor();
        }
        let cmd: DrawCmd = diff::diff(&self.back, &self.front);
        self.backend.render(&cmd)?;
        diff::promote(&mut self.back, &mut self.front);

        // Apply cursor state via the backend writer.
        use crossterm::{QueueableCommand, cursor};
        match self.cursor {
            CursorState::Hidden => {
                self.backend.writer().queue(cursor::Hide)?;
            }
            CursorState::Visible(x, y) => {
                self.backend.writer().queue(cursor::Show)?;
                self.backend.writer().queue(cursor::MoveTo(x, y))?;
            }
        }
        self.backend.writer().flush()?;
        Ok(())
    }

    /// Borrow the underlying writer (for alt-screen / raw-mode setup).
    pub fn writer(&mut self) -> &mut W {
        self.backend.writer()
    }

    /// Borrow the backend (for the app to call `invalidate` after a clear).
    pub fn backend(&mut self) -> &mut Backend<W> {
        &mut self.backend
    }

    /// Show the cursor.
    pub fn show_cursor(&mut self) -> io::Result<()> {
        use crossterm::{QueueableCommand, cursor};
        self.backend.writer().queue(cursor::Show)?;
        self.backend.writer().flush()?;
        Ok(())
    }

    /// Hide the cursor.
    pub fn hide_cursor(&mut self) -> io::Result<()> {
        use crossterm::{QueueableCommand, cursor};
        self.backend.writer().queue(cursor::Hide)?;
        self.backend.writer().flush()?;
        Ok(())
    }
}

/// A test terminal: owns a back grid the tests can render into and then
/// inspect, without any real I/O. Mirrors the `Terminal<TestBackend>` pattern
/// ratatui tests used. The grid is accessible via [`TestTerminal::buffer`], and
/// the last caret position the render closure requested via
/// [`TestTerminal::cursor`].
pub struct TestTerminal {
    back: Grid,
    cursor: CursorState,
}

impl TestTerminal {
    /// Create a test terminal with a grid of the given dimensions.
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            back: Grid::new(width, height),
            cursor: CursorState::default(),
        }
    }

    /// Run a draw closure against a frame over the back grid.
    pub fn draw<F>(&mut self, render: F)
    where
        F: FnOnce(&mut Frame<'_>),
    {
        let mut frame = Frame::new(&mut self.back);
        render(&mut frame);
        self.cursor = frame.take_cursor();
    }

    /// Read the rendered grid (the "buffer" the tests inspect).
    pub fn buffer(&self) -> &Grid {
        &self.back
    }

    /// The caret position the last draw closure requested (or `Hidden`).
    pub fn cursor(&self) -> CursorState {
        self.cursor
    }
}
