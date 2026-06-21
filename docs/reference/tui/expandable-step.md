# Expandable step

Shared rendering shape used by collapsible transcript entries: a one-line
summary header that toggles a body region open and closed. [Tool steps](tool-step.md)
and [thinking steps](thinking-step.md) are the two concrete instances. Both
render flat on the app background — there is no band or border; the header
is just a `+`/`-` marker plus summary text, and the body is indented content.
The color and toggle rules summarized below are the user-visible projection of
the formal [step state machine](step-state.md).

## Header

```text
  + Read crates/main.rs · 0ms
```

(The two leading spaces are the `app_bg` gutter shared by all transcript content.)

| Attribute | Value |
|-----------|-------|
| Background | `app_bg` (flat — no band, no border) |
| Inset | 2 cols of `app_bg` on each side (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD, at column 2 |
| Header text | Starts at column 4, BOLD, color set by the concrete step |
| Padding | Spaces fill the rest of the line with `app_bg` |

The marker sits at the start of the inset region; a single space separates it
from the header text. The 2-col `app_bg` gutters on each side match user
panels, code blocks, and markdown text, so steps never touch the terminal
frame.

## Body

```text
  - Read crates/main.rs · 0ms

    1  fn main() {
    2      ...
```

| Attribute | Value |
|-----------|-------|
| Background | `app_bg`; tool steps use a `code_bg` block only for the content itself |
| Body indent | 2 cols (transcript column 4) — left-aligned with the header text |
| Visibility | Rendered only when expanded |

The 2-col body indent is what makes the body line up with the header text in
`+ {header}`: the marker occupies column 2, the separating space column 3, and
the header text column 4 onward.

## Behavior

| Trigger | Effect |
|---------|--------|
| `↑` / `↓` (Browse zone) | Move keyboard focus to the previous / next visible step |
| `Enter` / `Space` on a focused **thinking** step | Toggle that step |
| `Enter` on a focused **tool** step | Open the [full-output detail overlay](tool-step.md#detail-overlay) (ADR-0001); click a tool-step header to toggle it inline instead |
| Click header | Focus and toggle that step |
| `Ctrl+T` | Expand or collapse all tool steps (bulk density toggle) |
| Sticky pin | When an expanded step's body scrolls past the top of the viewport, its header pins to the top row of the transcript area (rendered with `-`) |
| Narrow terminal (`< 8` cols) | Falls back to plain block rendering via `draw_message_body` |

Keyboard focus lives in the **Browse zone**. Press `Ctrl+B` in the input box
to enter Browse, then `↑` / `↓` to walk steps. Press any printable key
(typically `p` for "prompt") to return to the input. Mouse clicks use the same
semantic target model and also move focus to the clicked step.

### Header pinning on toggle

Toggling a step (expand or collapse) changes how many body lines sit below the
header, but the header's own content-line never moves.
`App::toggle_step_pinned` in `lib.rs` uses that to keep the header where the
user clicked:

| Case | Scroll behavior |
|------|-----------------|
| Header visible in viewport | Unchanged; the header stays on its row while the body grows or shrinks beneath it |
| Toggled via the sticky overlay | Set to the header's content-line, so the real header lands at row 0 where the overlay sat |
| Either case | `follow_bottom` is cleared so the next frame's auto-follow cannot push the header away |

The header's content-line is carried from the renderer via
`StickyInfo.header_line`.

## Concrete steps

| Step | Body | Source |
|------|------|--------|
| [Tool step](tool-step.md) | Tool-specific content (no labels), nested children | `draw_tool_step` |
| [Thinking step](thinking-step.md) | Wrapped reasoning text | `draw_reasoning_trace` |

## Click hit-testing

Each header records a `BlockRegion` in the layout map with a sentinel
`block_idx` so the click handler can tell the two step kinds apart:

| Step | `block_idx` sentinel |
|------|----------------------|
| Tool | `usize::MAX` |
| Thinking | `usize::MAX - 1` |

Regular text blocks use 0-based indices and never collide with these
sentinels.

`LayoutMap::interactive_targets()` returns the visible tool-step and thinking
targets in screen order for `↑` / `↓` navigation in Browse zone. Multiple hit
rows for the same step are deduplicated into one focus target.

## Source

Shared header rendering: `draw_expandable_step_header` and `tool_header_line`
in `crates/neenee-cli/src/tui/render/turn_artifacts.rs`. Sticky-pin tracking:
`StickyStep` in the same module. `BlockRegion` is defined in
`crates/neenee-cli/src/tui/layout.rs`. The structured output the tool-step body
renders from is documented in
[ADR-0001](../../adr/0001-tool-rendering-redesign.md).
