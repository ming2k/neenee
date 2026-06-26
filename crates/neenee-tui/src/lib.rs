//! In-house terminal rendering engine: a retained-mode cell grid with
//! write-marks-dirty tracking, a back/front grid diff, and a crossterm
//! backend that emits the minimal escape-code delta per frame.
//!
//! # Why this exists (ADR-0038)
//!
//! ratatui's model is *immediate mode*: every frame the entire UI is rebuilt
//! into a back buffer, then a cell-level diff against the previous frame
//! decides what bytes reach the terminal. That double-buffering optimizes
//! *transmission* but not *rebuilding* — layout, wrapping, and widget
//! construction still run for every cell every frame, even when nothing
//! changed. It also represents a double-width (CJK) glyph as a head cell plus
//! a trailing cell that it `reset()`s to `Color::Reset`, and its diff never
//! re-emits that trailing column, so through a multiplexer (tmux) the glyph's
//! background spillover goes stale and shows up as gray "ghost" blocks.
//!
//! This engine takes the vim/nvim approach instead:
//!
//! - A retained [`Grid`] is the single source of truth for what the
//!   application wants on screen. Writes mark the touched line dirty
//!   ([per-line `dirty_col`], vim's `ScreenGrid` model) at write time — no
//!   full-frame rescan.
//! - Each frame, [`Grid::diff`] compares the back grid (desired) against the
//!   front grid (what the terminal currently shows) and emits a stream of
//!   [`Draw`] commands: run-length packed cell runs with SGR-merged styles
//!   and cursor jumps over unchanged cells. Unchanged lines emit nothing.
//! - The application owns cell contents directly: a wide glyph's trailing
//!   column is filled with the glyph's own background by the writer, so
//!   ghost cells cannot occur regardless of terminal or multiplexer.
//! - When the terminal advertises `bce` (back-color-erase), line/region
//!   clears inherit the current background and the backend emits a single
//!   `clr_eol` (`\x1b[K`) instead of writing per-cell spaces — the cheap
//!   path vim and tmux both take.
//!
//! The engine is intentionally free of any application vocabulary: it knows
//! about cells, styles, and grids, never about transcripts, messages, or
//! tool steps. That keeps it independently testable (feed two grids, assert
//! the diff) and reusable.
//!
//! [per-line `dirty_col`]: Grid
//! [`diff`]: diff::diff
//! [`Draw`]: Draw

#![allow(dead_code)]

pub mod backend;
mod cell;
pub mod diff;
pub mod frame;
pub mod grid;
pub mod layout;
mod text;
pub mod widgets;

pub use cell::{Cell, Color, Modifier, Style};
pub use diff::{Draw, DrawCmd};
pub use frame::{CursorState, Frame, Terminal, Widget};
pub use grid::{Fit, Grid, Pos};
pub use layout::{Constraint, Direction, Layout, Margin, Rect};
pub use widgets::{Alignment, Block, BorderType, Borders, Clear, Line, Paragraph, Span, Wrap};
