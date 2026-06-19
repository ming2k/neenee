# Expandable card

Shared rendering shape used by collapsible UI elements: a one-line summary
header that toggles a body region open and closed. Tool-step and thinking
cards are the two concrete instances.

## Header

```text
  + Read crates/main.rs · 0ms
```

(The two leading spaces are the `app_bg` gutter shared by all transcript content.)

| Attribute | Value |
|-----------|-------|
| Background | `element_bg` (21, 23, 22) band, no border lines |
| Band inset | 2 cols of `app_bg` on each side (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD, at band column 0 (transcript column 2) |
| Header text | Starts at band column 2 (transcript column 4), BOLD, color set by the concrete card |
| Padding | Spaces fill the rest of the band with `element_bg` |

The marker sits at the start of the inset band; a single space separates it
from the header text. The 2-col `app_bg` gutters on each side match user
panels, code blocks, and markdown text, so cards never touch the terminal
frame.

## Body

```text
  - Read crates/main.rs · 0ms
   Tool
    read_file
```

| Attribute | Value |
|-----------|-------|
| Background | Set by the concrete card (`menu_bg` or `code_bg`) |
| Body indent | 2 cols inside the band (transcript column 4) — left-aligned with the header text |
| Visibility | Rendered only when expanded |

The 2-col body indent (inside the band) is what makes the body line up with
the header text in `+ {header}`: the marker occupies band column 0, the
separating space band column 1, and the header text band column 2 onward.

## Behavior

| Trigger | Effect |
|---------|--------|
| `Tab` | Move keyboard focus to the next visible card |
| `Shift+Tab` | Move keyboard focus to the previous visible card |
| `Enter` / `Space` on a focused **thinking** card | Toggle that card |
| `Enter` on a focused **tool-step** card | Open the [full-output detail overlay](tool-step-card.md#detail-overlay) (ADR-0001) — the inline expand/collapse for tool steps was replaced by the overlay; click a tool-step header to toggle it inline instead |
| Click header or preview | Focus and toggle that card |
| `Ctrl+T` | Expand or collapse all tool-step cards (bulk density toggle) |
| Sticky pin | When an expanded card's body scrolls past the top of the viewport, its header pins under the HUD bar (rendered with `-`) |
| Narrow terminal (`< 8` cols) | Falls back to plain block rendering via `render_message_blocks` |

Keyboard focus is the primary interaction path. Mouse clicks use the same
semantic target model and also move focus to the clicked card.

### Header pinning on toggle

Toggling a card (expand or collapse) changes how many body lines sit below
the header, but the header's own content-line never moves.
`App::toggle_card_pinned` in `lib.rs` uses that to keep the header where the
user clicked:

| Case | Scroll behavior |
|------|-----------------|
| Header visible in viewport | Unchanged; the header stays on its row while the body grows or shrinks beneath it |
| Toggled via the sticky overlay | Set to the header's content-line, so the real header lands at row 0 where the overlay sat |
| Either case | `follow_bottom` is cleared so the next frame's auto-follow cannot push the header away |

The header's content-line is carried from the renderer via
`StickyInfo.header_line`.

## Concrete cards

| Card | Body | Source |
|------|------|--------|
| [Tool-step card](tool-step-card.md) | Tool name, Arguments, Result, nested children | `render_tool_step_card` |
| [Thinking card](thinking-card.md) | Wrapped reasoning text | `render_thinking_card` |

## Click hit-testing

Each header records a `BlockRegion` in the layout map with a sentinel
`block_idx` so the click handler can tell the two card kinds apart:

| Card | `block_idx` sentinel |
|------|----------------------|
| Tool-step | `usize::MAX` |
| Thinking | `usize::MAX - 1` |

Regular text blocks use 0-based indices and never collide with these
sentinels.

`LayoutMap::interactive_targets()` returns the visible tool-step and thinking
targets in screen order for `Tab` / `Shift+Tab` navigation. Multiple hit rows
for the same card, such as collapsed previews, are deduplicated into one
focus target.

## Source

Shared header rendering: `draw_expandable_card_header` and
`tool_header_line` in `crates/neenee-tui/src/render/turn_artifacts.rs`.
Sticky-pin tracking: `StickyCard` in the same module. `BlockRegion` is
defined in `crates/neenee-tui/src/layout.rs`. The structured output the
tool-step body renders from is documented in
[ADR-0001](../../adr/0001-tool-rendering-redesign.md).
