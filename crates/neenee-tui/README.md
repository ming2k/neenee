# neenee-tui

In-house terminal rendering engine (ADR-0038): a retained-mode cell grid with
write-marks-dirty tracking, a back/front grid diff, and a crossterm backend
that emits the minimal escape-code delta per frame.

## Why this exists

ratatui's model is *immediate mode*: every frame the entire UI is rebuilt into
a back buffer, then a cell-level diff against the previous frame is emitted.
neenee's TUI needs finer control over IME composition windows, large diff
rendering, and incremental redraw, so this crate implements the grid + diff
engine directly on top of `crossterm` (raw mode, event stream, escape codes).

Consumed by `neenee-code` for the interactive coding agent interface.
