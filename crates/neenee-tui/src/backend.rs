//! The crossterm backend: turn a [`DrawCmd`] into the minimal escape-code
//! delta on stdout, with BCE (back-color-erase) awareness.
//!
//! # Responsibilities
//!
//! - Track the **current** cursor position and applied style across draws, so
//!   consecutive runs that share a style emit no SGR, and a run already at the
//!   right position emits no cursor move. This is the cell-level minimization
//!   vim's TUI frontend does.
//! - Detect `bce` from the `TERM`-derived terminfo capability and, when
//!   available, clear a dirty blank tail with `clr_eol` (`\x1b[K`) instead of
//!   writing per-cell spaces. Without `bce`, blank tails are painted as styled
//!   space cells (the only correct fallback).
//! - Own the crossterm `Write` sink. The engine never touches `stdout`
//!   directly except through this backend.
//!
//! The backend is the only place `crossterm` import leaks into the engine's
//! runtime path; the grid/diff modules stay pure and testable.

use std::io::{self, Write};

use crossterm::{
    QueueableCommand, cursor,
    style::{Attribute, Color as CtColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, ClearType},
};

#[allow(unused_imports)]
use crate::Cell;
use crate::diff::{Draw, DrawCmd};
use crate::{Color, Modifier, Style};

/// Whether the terminal advertises back-color-erase (`bce`).
///
/// When `Bce` is available, clearing a line tail to the current background is
/// a single `\x1b[K` that inherits the active bg. Without it, the backend
/// must write explicit space cells styled with the target background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bce {
    Yes,
    No,
}

impl Bce {
    /// Detect `bce` from the environment. This checks `TERM` against the set
    /// of terminals known to set the `bce` capability, plus an explicit
    /// override (`NEENEE_BCE=1` forces it on, `NEENEE_BCE=0` forces it off).
    ///
    /// We do not shell out to `tput`/`infocmp` (slow, not always present);
    /// the known-good list covers the terminals neenee targets (xterm,
    /// xterm-256color, foot, tmux, screen with BCE, kitty, wezterm, alacritty
    /// via its xterm emulation). This matches how other Rust TUI stacks
    /// approximate the capability.
    /// Detect `bce` from the environment (reads `TERM` and `NEENEE_BCE`).
    /// Shells out to the pure [`Bce::for_term`] helper.
    pub fn detect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let override_str = std::env::var("NEENEE_BCE").ok();
        Self::for_term(&term, override_str.as_deref())
    }

    /// Pure detection logic, separable from the environment for tests.
    ///
    /// `term` is the value of `TERM`; `override_str` is the optional
    /// `NEENEE_BCE` override (`"1"` forces on, `"0"` forces off). The
    /// known-bce set covers the terminals neenee targets (xterm, foot, tmux,
    /// screen with BCE, kitty, wezterm, alacritty via its xterm emulation).
    /// Unknown `TERM` values default to `No` so we never emit `clr_eol` to a
    /// terminal that won't honor the current bg.
    pub fn for_term(term: &str, override_str: Option<&str>) -> Self {
        if let Some(v) = override_str {
            return match v {
                "1" | "true" | "yes" => Bce::Yes,
                "0" | "false" | "no" => Bce::No,
                _ => Self::from_known(term),
            };
        }
        Self::from_known(term)
    }

    fn from_known(term: &str) -> Self {
        const BCE_TERMS: &[&str] = &[
            "xterm",
            "xterm-256color",
            "xterm-direct",
            "foot",
            "foot-extra",
            "kitty",
            "kitty-direct",
            "wezterm",
            "alacritty",
        ];
        let base = term.split('+').next().unwrap_or(term);
        if BCE_TERMS.contains(&base) || BCE_TERMS.contains(&term) {
            Bce::Yes
        } else {
            Bce::No
        }
    }
}

impl Default for Bce {
    fn default() -> Self {
        Self::detect()
    }
}

/// The crossterm-backed renderer. Owns the output writer and the
/// "what's currently on screen" tracking (cursor pos + last applied style),
/// so each draw emits only the delta.
pub struct Backend<W: Write> {
    out: W,
    bce: Bce,
    /// Last cursor position we moved to. `None` until the first move, which
    /// means the next draw must always reposition.
    cur: Option<(u16, u16)>,
    /// The style currently applied to the terminal (so we can skip redundant
    /// SGR sequences). Starts as the "unknown" default.
    style: Style,
}

impl<W: Write> Backend<W> {
    /// Wrap an output writer (typically `io::stdout()`), detecting `bce` from
    /// the environment.
    pub fn new(out: W) -> Self {
        Self::with_bce(out, Bce::detect())
    }

    /// Construct with an explicit `bce` setting (for tests / overrides).
    pub fn with_bce(out: W, bce: Bce) -> Self {
        Self {
            out,
            bce,
            cur: None,
            style: Style::RESET,
        }
    }

    /// Borrow the underlying writer (for the app to queue alt-screen, raw
    /// mode, etc. via crossterm directly).
    pub fn writer(&mut self) -> &mut W {
        &mut self.out
    }

    /// Apply a diff's draw commands: move the cursor into place, set the
    /// style delta, and write each run's symbols. Returns the number of draw
    /// commands processed.
    pub fn render(&mut self, cmd: &DrawCmd) -> io::Result<usize> {
        for draw in &cmd.draws {
            match draw {
                Draw::Cells { x, y, style, cells } => {
                    self.move_to(*x, *y)?;
                    self.apply_style(*style)?;
                    for (sym, w) in cells {
                        self.out.queue(crossterm::style::Print(sym.clone()))?;
                        // Advance our tracked cursor by the glyph's width; the
                        // trailing continuation column is implicit.
                        if let Some((cx, _cy)) = self.cur.as_mut() {
                            *cx = cx.saturating_add(*w as u16);
                        }
                    }
                }
                Draw::ClearEol { x, y, style, width } => {
                    self.move_to(*x, *y)?;
                    self.apply_style(*style)?;
                    let use_bce = matches!(self.bce, Bce::Yes) && style.bg == Color::Reset;
                    if use_bce {
                        // `\x1b[K` clears from the cursor to EOL with the
                        // currently-set background (which is default/reset here).
                        self.out.queue(terminal::Clear(ClearType::UntilNewLine))?;
                    } else {
                        // No BCE or non-default background: paint explicit styled spaces to the edge.
                        for _ in 0..*width {
                            self.out.queue(crossterm::style::Print(" "))?;
                        }
                        if let Some((cx, _cy)) = self.cur.as_mut() {
                            *cx = cx.saturating_add(*width);
                        }
                    }
                }
            }
        }
        Ok(cmd.draws.len())
    }

    /// Move the terminal cursor to `(x, y)` if we aren't already there.
    fn move_to(&mut self, x: u16, y: u16) -> io::Result<()> {
        if self.cur == Some((x, y)) {
            return Ok(());
        }
        self.out.queue(cursor::MoveTo(x, y))?;
        self.cur = Some((x, y));
        Ok(())
    }

    /// Apply only the style attributes that differ from the currently-applied
    /// style. Resets all attributes first when any attribute dropped, because
    /// SGR has no per-bit "off" that's universally cheaper than reset+reapply.
    fn apply_style(&mut self, want: Style) -> io::Result<()> {
        if want == self.style {
            return Ok(());
        }
        let have = self.style;
        // Foreground / background: only re-emit when changed.
        if want.fg != have.fg {
            self.out.queue(SetForegroundColor(to_ct_color(want.fg)))?;
        }
        if want.bg != have.bg {
            self.out.queue(SetBackgroundColor(to_ct_color(want.bg)))?;
        }
        // Attributes: if any bit dropped, reset all then reapply the wanted
        // set; if only bits were added, emit just the new ones.
        let dropped = have.add & !want.add;
        let added = want.add & !have.add;
        if !dropped.is_empty() {
            self.out.queue(SetAttribute(Attribute::Reset))?;
            // Re-assert colors too, since Reset also clears them.
            self.out.queue(SetForegroundColor(to_ct_color(want.fg)))?;
            self.out.queue(SetBackgroundColor(to_ct_color(want.bg)))?;
            for attr in iter_attrs(want.add) {
                self.out.queue(SetAttribute(attr))?;
            }
        } else {
            for attr in iter_attrs(added) {
                self.out.queue(SetAttribute(attr))?;
            }
        }
        self.style = want;
        Ok(())
    }

    /// Reset style/cursor tracking — call after the app does a wholesale
    /// screen clear or enters the alt screen, where the terminal's state no
    /// longer matches our tracked style.
    ///
    /// Crucially, this **emits a real SGR reset** (`\x1b[0m`) to the terminal,
    /// not just resets our in-memory tracking. Entering the alt screen *does*
    /// clear the terminal's SGR state, so a pure tracking reset is sufficient
    /// there — but a resize (tmux forwarding `SIGWINCH`, or a detach/reattach)
    /// does **not** touch the terminal's SGR: whatever attribute the previous
    /// frame last applied (often a bold tool-step summary line) stays on. If
    /// we only reset our tracker while the terminal keeps the old attribute,
    /// the next frame's delta-style computation (`apply_style`) sees equal
    /// attribute bits and emits nothing, so subsequent plain text renders with
    /// the stale attribute (e.g. the whole transcript reads as bold). Emitting
    /// the reset forces the real terminal back to RESET, keeping the tracker
    /// and the terminal honest with each other.
    pub fn invalidate(&mut self) -> io::Result<()> {
        self.out.queue(SetAttribute(Attribute::Reset))?;
        self.out.flush()?;
        self.cur = None;
        self.style = Style::RESET;
        Ok(())
    }
}

/// Map an engine [`Color`] to a crossterm color.
fn to_ct_color(c: Color) -> CtColor {
    match c {
        Color::Reset => CtColor::Reset,
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        Color::Black => CtColor::Black,
        Color::Red => CtColor::DarkRed,
        Color::Green => CtColor::DarkGreen,
        Color::Yellow => CtColor::DarkYellow,
        Color::Blue => CtColor::DarkBlue,
        Color::Magenta => CtColor::DarkMagenta,
        Color::Cyan => CtColor::DarkCyan,
        Color::Gray => CtColor::Grey,
        Color::DarkGray => CtColor::DarkGrey,
        Color::LightRed => CtColor::Red,
        Color::LightGreen => CtColor::Green,
        Color::LightYellow => CtColor::Yellow,
        Color::LightBlue => CtColor::Blue,
        Color::LightMagenta => CtColor::Magenta,
        Color::LightCyan => CtColor::Cyan,
        Color::White => CtColor::White,
    }
}

/// Translate the set modifier bits into crossterm `Attribute`s in a stable
/// order.
fn iter_attrs(m: Modifier) -> impl Iterator<Item = Attribute> {
    let mut v = Vec::new();
    if m.contains(Modifier::BOLD) {
        v.push(Attribute::Bold);
    }
    if m.contains(Modifier::DIM) {
        v.push(Attribute::Dim);
    }
    if m.contains(Modifier::ITALIC) {
        v.push(Attribute::Italic);
    }
    if m.contains(Modifier::UNDERLINE) {
        v.push(Attribute::Underlined);
    }
    if m.contains(Modifier::REVERSE) {
        v.push(Attribute::Reverse);
    }
    if m.contains(Modifier::STRIKETHROUGH) {
        v.push(Attribute::CrossedOut);
    }
    v.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Cell;
    use crate::grid::{Fit, Grid};

    /// Capture backend: accumulates the raw bytes emitted so tests can assert
    /// the exact escape sequence without a real terminal.
    fn render_to_string(cmd: &DrawCmd, bce: Bce) -> String {
        crossterm::style::force_color_output(true);
        let mut buf = Vec::new();
        {
            let mut be = Backend::with_bce(&mut buf, bce);
            be.render(cmd).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn empty_diff_emits_nothing() {
        let cmd = DrawCmd::default();
        assert_eq!(render_to_string(&cmd, Bce::Yes), "");
    }

    #[test]
    fn single_run_emits_move_sgr_and_text() {
        let mut back = Grid::new(4, 1);
        back.put(
            0,
            0,
            Fit::Clip,
            Style::default().fg(Color::Rgb(1, 2, 3)),
            "ab",
        );
        let front = Grid::new(4, 1);
        let cmd = crate::diff::diff(&back, &front);
        let s = render_to_string(&cmd, Bce::Yes);
        // crossterm emits RGB foreground as `\x1b[38;2;r;g;bm`.
        assert!(s.contains("\x1b[38;2;1;2;3m"), "fg SGR present: {s:?}");
        assert!(s.contains("ab"));
    }

    #[test]
    fn repeated_style_emits_no_duplicate_sgr() {
        // Two adjacent runs with the same style should emit the SGR once.
        let mut back = Grid::new(4, 1);
        let style = Style::default().fg(Color::Rgb(9, 9, 9));
        back.put(0, 0, Fit::Clip, style, "a");
        back.set(2, 0, Cell::narrow("b", style));
        let front = Grid::new(4, 1);
        let cmd = crate::diff::diff(&back, &front);
        let s = render_to_string(&cmd, Bce::Yes);
        // Count occurrences of the SGR set; should appear exactly once.
        let count = s.matches("\x1b[38;2;9;9;9m").count();
        assert_eq!(count, 1, "SGR emitted once, got: {s:?}");
    }

    #[test]
    fn bce_detection_defaults_for_known_terms() {
        assert_eq!(Bce::for_term("xterm-256color", None), Bce::Yes);
        assert_eq!(Bce::for_term("tmux-256color", None), Bce::No);
        assert_eq!(Bce::for_term("foot", None), Bce::Yes);
        assert_eq!(Bce::for_term("dumb", None), Bce::No);
        assert_eq!(Bce::for_term("unknown-term", None), Bce::No);
    }

    #[test]
    fn bce_override_env_wins() {
        // Override forces on/off regardless of TERM.
        assert_eq!(Bce::for_term("xterm-256color", Some("0")), Bce::No);
        assert_eq!(Bce::for_term("dumb", Some("1")), Bce::Yes);
        // Garbage override falls back to the TERM-based decision.
        assert_eq!(Bce::for_term("xterm-256color", Some("maybe")), Bce::Yes);
    }
}
