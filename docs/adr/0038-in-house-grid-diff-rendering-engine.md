# 0038. Replace ratatui with an in-house grid + diff rendering engine

- **Status:** Accepted (engine scaffolded; widget migration in progress)
- **Date:** 2026-06-27

## Context

ADR-0036 patched CJK wide-character ghost cells by adding a third full-screen
buffer (`WideHealBackend`) under ratatui. That wrapper exists *because* of two
ratatui properties we cannot fix from outside:

1. **Immediate-mode rebuild every frame.** ratatui's "double buffering" is a
   *transmission* optimization only: each frame the entire UI is rebuilt into
   a back buffer, then a cell-level diff against the previous frame decides
   what bytes reach the terminal. Layout, text wrapping, and widget
   construction still run for every cell every frame, even when nothing
   changed. There is no write-marks-dirty concept and no dirty-region skip —
   idle frames pay the full rebuild cost.
2. **The wide-char trailing cell is owned by the engine, not the application.**
   `Buffer::set_stringn` writes a width-2 glyph into the head cell and
   `reset()`s the trailing column to `Color::Reset`, and the diff never
   re-emits that column. So no application-layer write can give the trailing
   column the glyph's background; the only fix was a third buffer that
   re-emits changed rows wholesale.

Together these mean ratatui cannot express the model we want: a retained cell
grid the application writes to directly (owning wide-glyph trailing cells),
with write-time dirty tracking so the diff walks only what changed. vim and
nvim have had exactly this model (`ScreenGrid` + per-line `dirty_col`) for
decades, and it is why they never ghost and never waste work on idle frames.

We also want **`bce` (back-color-erase)** support: when the terminal
advertises `bce`, clearing a line tail to the current background is a single
`\x1b[K` instead of per-cell spaces. ratatui does not model terminal
capabilities; a grid we own can.

## Decision

Build a dedicated crate, `neenee-tui`, that replaces ratatui's `Terminal` +
`Buffer` layer with a vim-style retained grid + write-marks-dirty diff. It
sits on top of **crossterm** (raw mode, input, alt screen, escape emission)
— we keep crossterm for low-level I/O and replace only the buffer/diff layer
above it. This is the same split nvim takes (libtermkey/terminfo underneath,
its own grid layer above).

### Crate topology

```
crates/neenee-tui/        # new: grid + diff engine, zero app vocabulary
  src/
    lib.rs                # public API: Cell, Color, Style, Grid, diff, promote, Backend
    cell.rs               # Cell { symbol, width, style }; wide_continuation owns glyph bg
    text.rs               # grapheme width + CJK kinsoku line breaking (ported from render/text_layout.rs)
    grid.rs               # retained back/front grid + per-line dirty_col + dirty row range
    diff.rs               # back-vs-front diff -> run-length packed Draw::Cells (SGR-merged, cursor jumps)
    backend.rs            # crossterm I/O + Bce detection + style/cursor delta emission
```

Dependency flow is one-way: `neenee-code → neenee-tui → crossterm`. The engine
knows nothing about transcripts, messages, or tool steps — only cells, styles,
and grids — so it is independently testable and reusable.

### The four guarantees the engine provides

1. **Retained grid, write-marks-dirty.** `Grid::set` / `put` / `fill_rect`
   mark the touched line dirty from the changed column leftward, and extend a
   dirty row range, *at write time*. `diff` walks only the dirty rows from
   each row's `dirty_col` — no full-frame rescan. This is the
   `ScreenGrid`/`dirty_col` model.
2. **Back/front grid diff.** `diff(back, front)` compares the desired grid
   against a front grid mirroring what the terminal currently shows, emitting
   `Draw::Cells` runs: contiguous equal-style cells packed into one command
   (cursor move + one SGR set + one byte write). `promote` then copies dirty
   back cells into the front grid and clears dirty — the next frame against a
   stable state is a no-op (verified by `second_frame_after_promote_is_idle`).
3. **Wide-glyph ghost eradication at the source.** Writing a width-2 glyph
   also writes a `Cell::wide_continuation` carrying the glyph's background
   into the trailing column. The diff skips continuation cells (the head
   paints both columns), so the trailing column is never a stale
   `Color::Reset`. `WideHealBackend`'s third buffer is no longer needed.
4. **`bce` awareness.** `Bce::detect()` resolves the capability from `TERM`
   (known-bce set: xterm, foot, tmux, screen, kitty, wezterm, alacritty,
   rxvt) with a `NEENEE_BCE` override. Under `bce`, a dirty blank tail is
   `Draw::ClearEol` → one `\x1b[K` inheriting the active bg; without it, the
   diff emits explicit styled space cells (the only correct fallback).

### Backend style/cursor minimization

`Backend` tracks the currently-applied style and cursor position. `render`
moves the cursor only when the run isn't already at the target cell, and
emits only the SGR attributes that differ from the current state (dropped
bits trigger a reset+reapply; added bits are emitted alone). This is the
cell-level byte minimization nvim's TUI frontend does.

## What this does NOT change (yet)

- **Widget layer.** The tool-step / markdown / modal / diff / table / grep
  renderers currently in `neenee-code/src/tui/render/` (~5000 lines) still
  render *through ratatui*. Migrating them onto the new grid API is a
  separate, staged effort — one component at a time, keeping the app
  compiling throughout. The engine is built first and tested in isolation so
  the widget migration never has to debug the engine simultaneously.
- **Low-level terminal I/O.** crossterm stays for raw mode, input, alt
  screen, and escape-code emission. We did not write a terminfo parser.

## Alternatives considered

- **Keep ratatui, add a frame-level short-circuit (dirty flag around the whole
  frame).** Rejected as a half-measure: it skips transmission on idle frames
  but pays the full rebuild on any change, and it cannot fix the ghost-cell
  root cause (the trailing cell is still ratatui's to own). ADR-0036's third
  buffer would remain.
- **Keep ratatui, patch upstream `set_stringn`/diff.** Rejected: correct but
  out of our control and timeline; the immediate-mode rebuild cost remains
  regardless.
- **Write our own terminfo/escape layer too (drop crossterm).** Rejected: the
  value is in the grid/diff model, not in reimplementing terminal capability
  negotiation, input parsing, and alt-screen handling that crossterm already
  does correctly across targets.

## Consequences

- **Positive:** the three known rendering problems (full-frame rebuild cost,
  third-buffer ghost workaround, no `bce`) are addressed by one coherent
  model instead of three separate patches. The engine is pure and
  unit-testable (feed two grids, assert the diff) — no live terminal needed.
- **Positive:** `WideHealBackend` and its third full-screen buffer can be
  deleted once the widget layer migrates; ghost cells become impossible by
  construction.
- **Neutral:** the engine adds one crate and ~600 lines of carefully-tested
  core. The widget migration is the remaining large effort and is tracked
  separately.
- **Negative:** we now own grapheme-width, CJK line-breaking, and terminal
  behavior edge cases that ratatui absorbed. The kinsoku tables and width
  math are ported from the prior `text_layout.rs` so behavior is preserved,
  but the surface area is ours to maintain.

## References

- `crates/neenee-tui/` — the engine (`cell`, `text`, `grid`, `diff`,
  `backend`) and `tests/engine.rs` (the four-guarantee end-to-end tests).
- [ADR-0036](0036-cjk-wide-char-ghost-cells.md) — the third-buffer fix this
  engine supersedes.
- `crates/neenee-code/src/tui/wide_heal_backend.rs` — the wrapper to be
  retired after widget migration.
- neovim `src/nvim/grid_defs.h` / `screen.c` — `ScreenGrid`, per-line
  `dirty_col`, window `VALID`/`NOT_VALID` dirty states (the model this engine
  follows).
- [Terminal UI](../explanation/tui.md) — "Immediate-mode rendering" (the
  section to be revised to "retained grid" once migration lands).
