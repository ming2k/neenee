# Frame layout

## Overview

The viewport splits into three vertical chunks and the combined frame looks
like this. The transcript column always reclaims the full viewport width.

```text
    transcript column (full viewport width)
┌──────────────────────────────────────────────┐
│app_bg  ·  top viewport margin (1 row)         │
├──────────────────────────────────────────────┤
│Header (chunks[0])  ▄ top transition          │
│ model · goal · context bar  (panel_bg)       │
│Header  ▀ bottom transition                   │
├──────────────────────────────────────────────┤
│Transcript viewport (chunks[1])               │
│                                              │
├──────────────────────────────────────────────┤
│Footer (chunks[2])                            │
│ status? + input box + hint line              │
├──────────────────────────────────────────────┤
│app_bg  ·  bottom viewport margin (1 row)     │
└──────────────────────────────────────────────┘
```

The header is a floating `panel_bg` panel inset from both edges by `app_bg`
gutters, with half-block transitions — not a full-width band. The sections
below break the transcript column apart.

## Viewport

Every frame is first filled with `app_bg` so the TUI owns every cell rather
than leaving gaps at the terminal emulator's default color. Components then
render inside the **viewport**: `frame.size()` inset by `VIEWPORT_V_MARGIN = 1`
row top and bottom (`VIEWPORT_H_MARGIN = 0`, so components span the full
terminal width). The two margin rows are the only cells kept as pure `app_bg`
on every frame.

## Vertical chunks

ratatui's `Layout::default().direction(Vertical)` splits the viewport into
three chunks inside `draw_transcript`:

| Chunk | Constraint | Contents |
|-------|-----------|----------|
| Header | `Length(0)` modal / `Length(3)` / `Length(4)` with checklist | Half-block `panel_bg` panel: model name, goal, context-usage bar |
| Transcript | `Min(0)` | All message content |
| Footer | `Length(0)` modal / `status + input + 1` | Status bar, input box, hint line |

```text
┌──────────────────────────────────────────────────────────────┐
│app_bg  (top viewport margin, 1 row, outside the chunks)      │
├──────────────────────────────────────────────────────────────┤
│Header  ▄ top transition                             chunks[0]│
│ model name + goal + context-usage bar  (panel_bg)            │
│Header  ▀ bottom transition                                   │
├──────────────────────────────────────────────────────────────┤
│Transcript viewport  (all message content)           chunks[1]│
│                                                              │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│Footer  = status bar? + input box + hint line        chunks[2]│
│  status bar   (0 or 1 row)                                   │
│  input box    (2 + wrapped lines, capped at height / 2)      │
│  hint line    (1 row)                                        │
├──────────────────────────────────────────────────────────────┤
│app_bg  (bottom viewport margin, 1 row, outside the chunks)   │
└──────────────────────────────────────────────────────────────┘
```

## Footer

The footer (`chunks[2]`) stacks three rows from top to bottom. The status bar
is optional; the input box grows with its wrapped text; the hint line is always
one row. See [status-bar](status-bar.md) and [input-box](input-box.md) for
per-component detail.

```text
┌────────────────────────────────────────────────────────────┐
│Status bar (optional)                                       │
├────────────────────────────────────────────────────────────┤
│  e.g.  making edits                                        │
│Input box (grows with wrapped text)                         │
│  +--------------------------------------------+            │
│  |  type here...                              |            │
│  +--------------------------------------------+            │
├────────────────────────────────────────────────────────────┤
│Hint line (1 row, right-aligned keybindings)                │
│  e.g.  ^P paste   ^C cancel   ^S send                      │
└────────────────────────────────────────────────────────────┘
```

## Horizontal gutters

Every transcript-area component is inset by `TRANSCRIPT_H_INSET = 2` cols on each side so
no band, bar, or text touches the terminal frame. The two gutters stay
`app_bg` via the global frame fill. Solid-background bands (card headers and
bodies, child tool steps) render into `transcript_band_rect` (`render/mod.rs`), which
is the transcript area minus both gutters; user panels and code blocks render their
own equivalent gutters; markdown text wraps with `TRANSCRIPT_H_INSET` cells of slack
on the right.

```text
┌──────────────────────────────────────────────────────────────┐
│columns: 0 1 2 3                                 ... W-1      │
│          v v v v                                 v           │
│                                                              │
│          app_bg |    transcript band              | app_bg   │
│                                                              │
│          ..  .. +-------------------------------+ ..  ..     │
│          ..  .. |  card header / body / text    | ..  ..     │
│          ..  .. +-------------------------------+ ..  ..     │
│                                                              │
│          <- INSET=2 ->|<-- usable width -->|<- INSET=2 ->    │
└──────────────────────────────────────────────────────────────┘
```

## Chrome hiding

When an overlay modal is open, `chrome_hidden = true` collapses the header and
footer heights to 0. The modal gets the full viewport with no header, input
box, status bar, or hint line visible.

```text
┌────────────────────────────────────────────────────────────┐
│viewport owned by modal + dim backdrop                      │
│                                                            │
│        +-------------------------------+                   │
│        |                               |                   │
│        |     centered overlay modal    |                   │
│        |                               |                   │
│        +-------------------------------+                   │
│                                                            │
│header = 0,  footer = 0                                     │
└────────────────────────────────────────────────────────────┘
```

Modal types that hide chrome: Models, Sessions, Help, Permission.
Modal types that keep chrome: None, ApiKey, Endpoint, ModelName, HistorySearch.

## Transcript viewport behavior

- Messages render top-to-bottom with 1-row spacing between them.
- Auto-follow pins to the newest content.
- Scrolling up pauses follow; scrolling back to the bottom (or sending a
  message) re-engages it.

## Key measurements

| Measurement | Value | Where |
|------------|-------|-------|
| Left/right gutter (all transcript content) | 2 cols `app_bg` | `TRANSCRIPT_H_INSET`, applied via `transcript_band_rect` (cards) / explicit spans (user panel, code block) / wrap-width slack (markdown) |
| `┃` bar column | 2 (after 2-col gutter) | User messages, code blocks, input |
| Assistant text indent | 4 cols (left) + 2-col right gutter | `line_spans("    ", ...)`; wraps at `area.width - 6` |
| Code block indent | 2 cols (inside band) + `┃` + space | `code_gutter_line(left_indent=2)` |
| Card marker column | 2 (inside `TRANSCRIPT_H_INSET` band) | `+` / `-` at band col 0 in `card_header_line` |
| Card header text column | 4 (2 gutter + 2 after `+ `) | After `+ ` prefix |
| Card body indent | 4 cols from transcript edge (2 inside band) | `draw_tool_body_section`, `draw_reasoning_trace` |
| Line-number gutter min width | 2 chars | `.max(2)` |
| Mouse scroll step | 4 rows | `ScrollUp/Down` handler |
| PageUp/PageDown step | `view_height - 1` | One line of overlap |
| Input box max height | `terminal_height / 2` | Capped so transcript stays visible |
| Message spacing | 1 row | Between consecutive messages |
