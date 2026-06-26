# 0036. Heal CJK wide-character "ghost" cells with a whole-row re-emitting backend

- **Status:** Accepted
- **Date:** 2026-06-26

## Context

Scrolling a transcript that contains CJK (double-width) text under
**foot + tmux** leaves stray gray blocks in the columns next to the glyphs.
They line up vertically at the wrap column (one per wide glyph at a line's end)
and shift as the view scrolls.

Tracing it to ground truth (a `TestBackend` grid dump) established the
mechanism — two ratatui behaviours compounding with the multiplexer:

1. **The trailing column is left transparent.** ratatui represents a
   double-width glyph as a head cell plus a trailing cell.
   `ratatui_core::buffer::Buffer::set_stringn` writes the glyph into the head
   and `reset()`s the trailing cell, leaving its background on `Color::Reset`
   (= "use the terminal's default background") rather than the glyph's surface.

2. **The diff never re-emits the trailing column.** `buffer/diff.rs` advances
   `pos += cell_width - 1` past the trailing column, trusting the glyph to
   repaint both columns' background. So no application-layer write can put those
   cells on screen — the diff drops them before the backend.

3. **tmux realises the artifact.** On a bare, compliant terminal (foot alone)
   the glyph paints both columns, so the trailing cell is covered. tmux keeps
   its own grid and, on scroll/partial redraws, paints that trailing column with
   *its* default background. The user confirmed the blocks are exactly foot's
   background colour: they are `Color::Reset` cells resolving to the terminal
   default, which differs from neenee's near-black `Theme::surface()`
   (`#070808`), so they read as gray flecks.

The comparison that settled the design: **opencode does not have this bug**, and
it *also* paints a full custom background. The difference is purely the
renderer — opencode uses OpenTUI (a full-framebuffer renderer that repaints
every cell), not ratatui. So this is not a terminal law or a "don't paint a
background" rule; it is ratatui's cell-level-diff choice, and it is fixable on
our side.

## Decision

Wrap the crossterm backend in `WideHealBackend` (`tui/wide_heal_backend.rs`),
which upgrades ratatui's *changed-cell* output to *changed-row* output — the
same "repaint the whole line" strategy OpenTUI uses:

- Keep a full **shadow** `Buffer` mirroring the screen, accumulated from every
  `draw`.
- On each `draw`, apply the diff's changed cells to the shadow and record which
  rows were touched. Then re-emit each touched row **in full**, left to right,
  skipping the trailing column of every wide glyph (advance by the glyph's
  display width — the glyph repaints that column itself).
- Unchanged rows are not re-emitted, so idle frames stay silent (no flicker, no
  busy output); cursor, resize, and diffing all stay in ratatui's hands.

Because every wide glyph in a changed row is reprinted with its real background
each frame the row changes, the background spillover is kept fresh and tmux can
never leave a stale trailing cell.

## Alternatives considered

- **Pin the terminal default background to the surface via OSC 11.** Rejected:
  tmux swallows OSC 11 (it does not forward set-background to the host terminal
  without passthrough), so it never reaches foot; and even if it did, it would
  set foot's background, not tmux's pane background. Inert under the very setup
  that has the bug.
- **Heal the front buffer (fold each trailing `Reset` into its neighbour's
  background).** Rejected: proven inert with `TestBackend` — the wide-char diff
  skips emitting those cells regardless of their buffer contents, so the heal
  never reaches the screen.
- **Use `Color::Reset` for the base surface (inherit the terminal background).**
  Rejected: it removes the mismatch for plain prose but abandons the curated
  near-black surface, and CJK inside colored bands (code/body, panels) would
  still ghost.
- **Force a full-screen repaint every frame (`terminal.clear()`).** Rejected:
  the diff drops wide-char trailing columns even against an empty buffer, so it
  would not help, and it flickers.

## Consequences

- **Positive:** the ghost blocks are fixed at the source — fresh per-frame
  glyph repaint of changed rows — and it holds through tmux and on any terminal,
  matching opencode's robustness. The change is localized to one backend
  wrapper; the `Terminal` render loop, cursor, and resize paths are untouched.
- **Neutral:** a changed row is re-emitted in full rather than cell-by-cell, so
  frames that touch many rows (e.g. scrolling) emit more bytes than ratatui's
  minimal diff. This is bounded by `changed_rows × width` and is the same
  tradeoff full-framebuffer renderers make; idle frames still emit nothing.
- **Negative / future:** the real bug is upstream — `set_stringn` should give a
  wide glyph's continuation cell the glyph's background instead of `reset()`-ing
  it to `Reset`, and/or the diff should not silently drop it. Worth filing
  upstream so the wrapper can eventually be retired.

## References

- `crates/neenee-code/src/tui/wide_heal_backend.rs` — the wrapper + tests.
- `crates/neenee-code/src/tui/mod.rs` — `run_tui` wraps `CrosstermBackend`.
- `ratatui-core` `buffer/buffer.rs::set_stringn` (trailing-cell `reset()`) and
  `buffer/diff.rs` (wide-char trailing-column skip).
- opencode `packages/tui` — OpenTUI (`@opentui/core`) full-framebuffer renderer,
  the prior art that proved the fix belongs in the renderer.
- [Terminal UI](../explanation/tui.md) — "Terminal underpinnings".
