# Frame layout

How the terminal rect is divided across the TUI's three viewing modes: the
**root conversation**, the **sub-agent zoom view**, and the **modal overlay**
state. Component-by-component detail lives on each component's own page;
this one owns the rect math, the chrome-hiding rules, and the measurements
table.

## Viewport

Every frame is first filled with `theme.surface()` (`app_bg`) so the TUI
owns every cell rather than leaving gaps at the terminal emulator's default
color. Components then render inside the **viewport**: `frame.size()`
inset by `VIEWPORT_V_MARGIN = 1` row top and bottom (`VIEWPORT_H_MARGIN = 0`,
so components span the full terminal width). The two margin rows are the
only cells kept as pure `app_bg` on every frame.

```text
┌──────────────────────────────────────────────────────────────┐
│app_bg  (top viewport margin, 1 row — outside every chunk)    │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│                   viewport (everything below)                │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│app_bg  (bottom viewport margin, 1 row — outside every chunk)│
└──────────────────────────────────────────────────────────────┘
```

The viewport rect itself comes from `viewport_rect(frame)` in
`crates/neenee-cli/src/tui/render/primitives.rs`.

## Root conversation view

The default. A two-chunk vertical split inside `draw_transcript`:

| Chunk | Constraint | Contents |
|-------|-----------|----------|
| Transcript | `Min(0)` | All message content; sticky-pinned step summaries overlay its top row |
| Footer | `Length(footer_height)` | A vertical stack (see below) |

```text
┌──────────────────────────────────────────────────────────────┐
│app_bg  (top viewport margin, 1 row)                          │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  Transcript viewport                              chunks[0]  │
│   (messages, expandable steps, sticky pinned summaries)      │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│  Status bar (optional, 0 or 1 row)                ┐          │
│  Plan panel (optional, 0 or 3 rows)               │          │
│  Input box (grows with text, capped)              │ chunks[1]│
│  Hint bar (1 row, persistent)                     ┘          │
├──────────────────────────────────────────────────────────────┤
│app_bg  (bottom viewport margin, 1 row)                       │
└──────────────────────────────────────────────────────────────┘
```

There is **no top header**. The model name, goal, MCP summary, and
context-usage indicator that a header would carry live in the
[hint bar](hint-line.md) at the bottom, so the transcript reclaims the full
vertical space above the footer.

### Footer stack

The footer's height is the sum of its four rows. Each row is independently
optional and the stack collapses from the top when a row is hidden:

| Row | Height | When present |
|-----|--------|--------------|
| Status bar | `STATUS_BAR_ROWS = 1` | Activity is non-empty and not `idle` / `responding`; not in sub-agent view; chrome visible |
| Plan panel | `PLAN_PANEL_ROWS = 3` | A plan is active; not in sub-agent view; chrome visible |
| Input box | `COMPOSER_VERTICAL_CHROME_ROWS + wrapped_lines`, capped at `terminal_height / 2`, min `COMPOSER_MIN_HEIGHT = 3` | Not in sub-agent view; chrome visible |
| Hint bar | `HINT_BAR_ROWS = 1` | Chrome visible (always, when no modal is open) |

```text
┌────────────────────────────────────────────────────────────┐
│ ● making edits                                  ← status bar│
├────────────────────────────────────────────────────────────┤
│ ╭── Plan: docs/plan.md · 2/5 done ───────────────────╮     │
│ │ ✓ extract session store   ▸ migrate tests   · pending │     │
│ ╰────────────────────────────────────────────────────╯     │  ← plan panel
├────────────────────────────────────────────────────────────┤
│  > type here…                                              │  ← input box
├────────────────────────────────────────────────────────────┤
│ [ COMPOSE ]            Kimi K2.7 Code   89.2k (8%)         │  ← hint bar
└────────────────────────────────────────────────────────────┘
```

The footer is inset by `FOOTER_H_INSET = TRANSCRIPT_H_INSET = 2` cols on
each side; all four rows share the same horizontal extent so their left and
right edges line up.

### Sticky pinned step summary

When an expanded step's body covers the top of the viewport (its summary
has scrolled out of view), the renderer overlays the step's one-line
summary on the top row of the transcript area with a `-` marker. This lets
the user always see which step's body they are looking at, and click to
collapse it, without forcing a scroll anchor. Rendered by
`draw_sticky_summary_if_needed`; see [expandable step](expandable-step.md).

## Sub-agent zoom view

When the user zooms into a `task` tool step, the footer is hidden entirely
and the transcript chunk is split to make room for a one-row navigation bar
at the bottom. The message stream is the focused task's child messages,
not the root conversation.

```text
┌──────────────────────────────────────────────────────────────┐
│app_bg  (top viewport margin, 1 row)                          │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  Transcript viewport (focused task's child messages)         │
│                                                              │
│   …user / assistant / tool steps / thinking steps…           │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│  Task  explore the codebase  (1 of 3)   Esc back  [ prev  ] next │  ← sub-agent bar
├──────────────────────────────────────────────────────────────┤
│app_bg  (bottom viewport margin, 1 row)                       │
└──────────────────────────────────────────────────────────────┘
```

| Region | Constraint | Height |
|--------|-----------|--------|
| Transcript (children) | `Min(0)` | fills |
| Sub-agent bar | `Length(SUBAGENT_BAR_ROWS = 1)` | 1 |

Status bar, plan panel, input box, and hint bar all collapse to 0 — the
zoomed view is read-only, with the navigation bar as its only chrome. See
[Sub-agent view](subagent-view.md) for the focus stack that drives this
mode and the bar's contents.

## Modal overlay view

When an overlay modal is open, `chrome_hidden = true` collapses the entire
footer (status bar, plan panel, input box, hint bar) to 0 height. The
modal takes over the viewport with a dim backdrop painted over whatever
the transcript was showing. The one exception is the
[permission sheet](modals.md#permission-sheet), which is inline (no
backdrop, no `chrome_hidden`) and replaces only the input-box area.

```text
┌──────────────────────────────────────────────────────────────┐
│                                                              │
│             dim backdrop over the frozen transcript          │
│                                                              │
│            ╭────────────────────────────────────╮            │
│            │                                    │            │
│            │       centered overlay modal       │            │
│            │                                    │            │
│            ╰────────────────────────────────────╯            │
│                                                              │
│footer = 0 (status bar, plan panel, input box, hint bar all   │
│           hidden)                                            │
└──────────────────────────────────────────────────────────────┘
```

See [modals](modals.md) for which modal uses which `centered_rect`
percentage and which (rare) overlays keep the chrome visible.

## Horizontal gutters

Every transcript-area component is inset by `TRANSCRIPT_H_INSET = 2` cols
on each side so no band, bar, or text touches the terminal frame. The two
gutters stay `app_bg` via the global frame fill. Solid-background regions
(code blocks, child tool steps) render into `transcript_band_rect`
(`render/mod.rs`), which is the transcript area minus both gutters; user
panels and code blocks render their own equivalent gutters; markdown text
wraps with `TRANSCRIPT_H_INSET` cells of slack on the right.

```text
┌──────────────────────────────────────────────────────────────┐
│columns: 0 1 2 3                                 ... W-1      │
│          v v v v                                 v           │
│                                                              │
│          app_bg |    transcript band              | app_bg   │
│                                                              │
│          ..  .. +-------------------------------+ ..  ..     │
│          ..  .. |  step header / body / text     | ..  ..     │
│          ..  .. +-------------------------------+ ..  ..     │
│                                                              │
│          <- INSET=2 ->|<-- usable width -->|<- INSET=2 ->    │
└──────────────────────────────────────────────────────────────┘
```

The footer shares the same inset (`FOOTER_H_INSET = TRANSCRIPT_H_INSET`),
so the status bar, plan panel, input box, and hint bar all line up with
the transcript content above.

## Transcript viewport behavior

- Messages render top-to-bottom with `MESSAGE_GAP_ROWS = 1` row of spacing
  between them.
- Auto-follow pins to the newest content while `follow_bottom` is set.
- Scrolling up pauses follow; scrolling back to the bottom (or sending a
  message) re-engages it.
- `PageUp` / `PageDown` step by `view_height - 1` (one line of overlap);
  mouse wheel steps by 4 rows.

## Key measurements

| Measurement | Value | Where |
|------------|-------|-------|
| Top/bottom viewport margin | 1 row each (`app_bg`) | `VIEWPORT_V_MARGIN` |
| Left/right viewport margin | 0 cols | `VIEWPORT_H_MARGIN` |
| Left/right gutter (all content) | 2 cols `app_bg` | `TRANSCRIPT_H_INSET`, applied via `transcript_band_rect` (steps) / explicit spans (user panel, code block) / wrap-width slack (markdown) |
| Footer side inset | 2 cols (matches `TRANSCRIPT_H_INSET`) | `FOOTER_H_INSET` |
| Status bar height | 1 row | `STATUS_BAR_ROWS` |
| Plan panel height | 3 rows (top border + content + bottom border) | `PLAN_PANEL_ROWS` |
| Hint bar height | 1 row | `HINT_BAR_ROWS` |
| Sub-agent bar height | 1 row | `SUBAGENT_BAR_ROWS` |
| Input box min height | 3 rows (top transition + 1 text + bottom transition) | `COMPOSER_MIN_HEIGHT` |
| Input box max height | `terminal_height / 2` | `COMPOSER_MAX_HEIGHT_DIVISOR` |
| Input box vertical chrome | 2 rows (top + bottom transition) | `COMPOSER_VERTICAL_CHROME_ROWS` |
| Input box left prefix | 2 cols (`>` + space, or wrap-aligned indent) | `COMPOSER_PROMPT_PREFIX_COLS` |
| Input box right pad | 2 cols | `COMPOSER_RIGHT_PAD_COLS` |
| `┃` bar column | 2 (after 2-col gutter) | User messages, code blocks, input |
| Assistant text indent | 4 cols (left) + 2-col right gutter | `TRANSCRIPT_BODY_PREFIX_COLS`; wraps at `area.width - 6` |
| Code block indent | 2 cols (inside band) + `┃` + space | `code_gutter_line(left_indent=2)` |
| Step marker column | 2 (inside `TRANSCRIPT_H_INSET` band) | `+` / `-` at col 0 of the inset region |
| Step header text column | 4 (2 gutter + 2 after `+ `) | After `+ ` prefix |
| Step body indent | 4 cols from transcript edge | `draw_tool_step`, `draw_reasoning_trace` |
| Line-number gutter min width | 2 chars | `.max(2)` |
| Message spacing | 1 row between consecutive messages | `MESSAGE_GAP_ROWS` |
| Mouse scroll step | 4 rows | `ScrollUp`/`Down` handler |
| PageUp/PageDown step | `view_height - 1` | One line of overlap |

## Source

| File | Responsibility |
|------|----------------|
| `render/mod.rs` | `draw_transcript` — viewport fill, two-chunk split, footer stack, sub-agent split, sticky summary overlay |
| `render/design.rs` | All non-color layout tokens: `VIEWPORT_*`, `TRANSCRIPT_*`, `FOOTER_H_INSET`, `STATUS_BAR_ROWS`, `PLAN_PANEL_ROWS`, `HINT_BAR_ROWS`, `SUBAGENT_BAR_ROWS`, `COMPOSER_*`, `MESSAGE_GAP_ROWS` |
| `render/primitives.rs` | `viewport_rect`, `centered_rect`, `panel_block`, `draw_dim_backdrop` |
| `render/chrome.rs` | `draw_status_bar`, `draw_hint_bar` / `HintBarView` |
| `render/composer.rs` | `draw_composer` (input box), `INPUT_MSG_IDX` |
| `render/message_body.rs` | `draw_plan_panel` |
| `render/step/renderers.rs` | `draw_subagent_bar`, `draw_sticky_summary_if_needed` |
| `app.rs` | `in_subagent_view`, `focus_stack`, `follow_bottom`, scroll clamping |
