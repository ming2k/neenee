# Modals

Centered overlays that take over the viewport until dismissed. With one
explicit exception (the [permission sheet](#permission-sheet), which is
inline), every modal hides the regular chrome — header, status bar, input
box, hint line — by setting `chrome_hidden` for the duration of the draw.

## Shared chrome

Every centered modal goes through the same primitives in
`crates/neenee-cli/src/tui/render/primitives.rs`:

- `draw_dim_backdrop(frame, frame.size(), theme.backdrop())` paints a
  full-frame dim layer so the underlying transcript recedes.
- `centered_rect(px_w, px_h, viewport)` carves the modal rectangle out of
  the viewport (the frame minus the global 1-row top/bottom margin). The
  surrounding gutters are kept as `app_bg`.
- `modal_frame(area, theme.panel(), header, footer)` produces a borderless
  solid-bg panel with 2-col horizontal and 1-row vertical inner padding,
  vertically split into `header(Length 1) → gap(Length 1) → body(Min 0) →
  gap(Length 1) → footer(Length 1)`. Header/footer/gap rows are omitted when
  not requested.

```text
                ┌──── centered_rect(px_w, px_h) ────┐
   app_bg gutter│ ▄▄ modal border (top transition) │app_bg gutter
                │  Header  ·  brand+muted           │
                │                                   │
                │  Body  (scrollable, follow=sel.)  │
                │                                   │
                │  Footer  ·  muted                 │
   app_bg gutter│ ▀▀ modal border (bot transition) │app_bg gutter
                └───────────────────────────────────┘
```

The two non-`modal_frame` exceptions — the [tool-step detail overlay](#tool-step-detail-overlay)
and the [plan preview](#plan-preview-modal) — manage their own `Paragraph`
scroll directly. The two [toasts](#toasts) are non-modal and use a different
`toast` helper.

## Overview

| Modal | Trigger | `centered_rect` | Source |
|-------|---------|-----------------|--------|
| [Models](#models-modal) | `Ctrl+M` / `/models` | 72 × 60 | `draw_models_modal` |
| [Model editor](#model-editor) | Models modal `e` | 60 × 36 | `draw_model_editor` |
| [Sessions](#sessions-modal) | `/sessions` | 80 × 64 | `draw_sessions_modal` |
| [Session](#session-modal) | `/session` | 76 × 70 | `draw_session_modal` |
| [History search](#history-search-modal) | `Ctrl+R` | 70 × 55 | `draw_history_modal` |
| [Question](#question-modal) | `ask_user` tool | 78 × 70 | `draw_question_modal` |
| [Permission sheet](#permission-sheet) | Automatic | (inline, not centered) | `draw_permission_sheet` |
| [Tool-step detail](#tool-step-detail-overlay) | `Enter` on focused tool step | 92 × 84 | `draw_tool_step_detail_overlay` |
| [Help](#help-modal) | `Ctrl+H` / `/help` | 58 × 70 | `draw_help_modal` |
| [Plan preview](#plan-preview-modal) | `Ctrl+P` / click plan panel | 80 × 70 | `draw_plan_preview_modal` |
| [Toasts](#toasts) | Transient | top-right, 3 rows | `draw_armed_toast`, `draw_copy_toast` |

## Closing

- `Esc` or `Ctrl+C` closes most modals.
- Permission sheet: `Esc` rejects; `Ctrl+C` closes and rejects.
- Model editor: `Ctrl+C` restores the stashed composer input and exits the
  configuration flow.

## Models modal

Provider/model picker (ADR-0002 phase 3). Borrows the composer input as a
fuzzy filter. Rows are ranked favorites-first, then last-used, then name.

```text
╭───────────────────────────────────────────────╮
│ Models  ❯ openai                              │  ← header (real caret here)
│                                               │
│  ★  ●  openai       ✓  gpt-4o   · description │  ← selected → brand bg
│      ●  anthropic    ✗  claude…  · description│
│  ★     google       ✓  gemini…  · description│
│      …                                         │
│                                               │
│ type to filter · ↑↓ navigate · enter activate │
│ * favorite · esc                              │
╰───────────────────────────────────────────────╯
```

| Key | Effect |
|-----|--------|
| printable | Append to the filter (composer is the input source) |
| `↑` / `↓` | Move selection |
| `Enter` | Activate the highlighted row, or the default on empty filter |
| `*` | Toggle favorite on the highlighted row |
| `Esc` | Close |

`Ctrl+M` opens this modal only on terminals that support the Kitty enhanced
keyboard protocol. In a raw terminal `Ctrl+M` is byte-identical to `Enter`,
so on unsupported terminals the key falls through to `Enter` and `/models`
is the reliable trigger.

## Model editor

Unified API-key + model-id editor (ADR-0002 phase 4). Two fields with `Tab`
cycling focus; the composer input is the value of the focused field, the
unfocused one is held in a buffer.

```text
╭───────────────────────────────────╮
│ Edit · openai                     │
│                                   │
│  API key   ••••••••••••••••       │  ← muted (unfocused, masked)
│  Model id  gpt-4o                 │  ← bold brand label (focused, caret)
│                                   │
│ tab switch field · enter save · esc cancel │
╰───────────────────────────────────╯
```

The API key is masked as `•` per character whenever it is not focused.

| Key | Effect |
|-----|--------|
| printable | Append to the focused field |
| `Tab` | Cycle focus between API key and Model id |
| `Enter` | Save the focused field and switch to the other |
| `Esc` / `Ctrl+C` | Cancel and restore the stashed composer input |

## Sessions modal

Sessions picker. Each row shows an overview plus created/active relative
times; `Enter` resumes the selected session.

```text
╭──────────────────────────────────────────────────────────╮
│ Sessions                                                 │
│                                                          │
│ ●  fix login redirect bug      created 2h · active 3m    │  ← active + selected
│    refactor database layer     created 1d · active 5h    │
│    write API docs              created 3d · active 2d    │
│                                                          │
│ ↑↓ navigate · Enter open · d delete · Esc close          │
╰──────────────────────────────────────────────────────────╯
```

The `●` badge marks the currently active session. Overview text is
truncated with `…` when it would collide with the meta column.

## Session modal

Tabbed live-session context viewer, opened by `/session`. Five tabs:
**Model**, **Mcp**, **Skills**, **Permissions**, **Tools**. List panes
(Skills / Permissions / Tools) keep the selected row in view via the
shared `follow` mechanism.

```text
╭──────────────────────────────────────────────────────╮
│ Session    Model  Mcp  Skills  Permissions  Tools     │  ← active tab underlined
│                                                      │
│  Provider     openai                                 │
│  Model        GPT-4o  (gpt-4o-2024-08-06)            │
│  Context      128k tokens                            │
│  API key      ✓ ready                                │
│  Capabilities  tools · vision                        │
│                                                      │
│ ← → switch tab [· ↑↓ select · Space act][· ↑↓ scroll]│
│ · Esc close                                          │
╰──────────────────────────────────────────────────────╯
```

The footer hint adapts to the tab: interactive panes (Permissions / Tools)
advertise `↑↓ select · Space act`; the others advertise `↑↓ scroll`.
Placeholders show `Loading…` until the snapshot arrives.

| Key | Effect |
|-----|--------|
| `←` / `→` | Switch tab |
| `↑` / `↓` | Scroll (read-only panes) or move selection (interactive panes) |
| `Space` | Act on the selected row (interactive panes only) |
| `Esc` | Close |

## History search modal

Input-history fuzzy search. Borrows the composer input as the query.

```text
╭──────────────────────────────────────────────────╮
│ Input History  ❯ open                            │  ← caret here
│                                                  │
│   1  /mode plan                                  │
│   2  h̲o̲w̲ do I open the file?                    │  ← matched chars branded
│   3  explain t̲h̲i̲s̲ function                     │
│                                                  │
│ type to filter · ↑↓ navigate · Enter insert · Esc│
╰──────────────────────────────────────────────────╯
```

Characters whose char-index is in `FuzzyMatch.positions` are styled
differently (brand + bold when unselected, contrast + underlined when
selected) so the user sees why each entry surfaced. Fuzzy ranking is
pre-computed by `App::history_filtered` to avoid a second pass per frame.

## Question modal

Centered modal for `UserQuestionRequest`. Presents one question at a time
with options (single- or multi-select), plus a built-in **Other** option
that exposes a free-text input.

```text
╭────────────────────────────────────────────────────────╮
│ Question 1/2                                           │
│                                                        │
│  Which framework?                                      │
│                                                        │
│ ❯ 1.  ● React   — component-based                     │  ← highlighted
│                                                        │
│   2.  ○ Vue    — progressive                           │
│                                                        │
│   3.  ○ Other  — Type your own answer                  │
│       ▏ Type your own answer                          │  ← revealed on Other
│                                                        │
│ ↑↓ navigate · Space toggle · 1-9 jump · Enter submit · │
│ Esc cancel                                             │
╰────────────────────────────────────────────────────────╯
```

`[x]` / `[ ]` mark multi-select; `●` / `○` mark single-select. The leading
digit prefix (`1.`–`8.`) advertises the 1-9 jump shortcut.

| Key | Effect |
|-----|--------|
| `↑` / `↓` | Move highlight (last row is always **Other**) |
| `1`–`9` | Jump to the Nth option |
| `Space` | Toggle the highlighted option |
| `Enter` | Submit the answer(s) |
| `Esc` | Cancel the question |

See [User questions](../../explanation/agent-design/user-questions.md) for
how the agent side blocks on the answer.

## Permission sheet

Blocking tool-permission prompt rendered **inline**, replacing the composer
(input-box) area; the transcript above stays visible. It is the only modal
without a backdrop or centered rect. Collapsed by default; expanding
**Details** grows the body upward into the transcript, up to
`PERMISSION_MAX_BODY_ROWS = 14`.

```text
… transcript (visible, scrollable above) …

┃ Run shell command  src/main.rs
┃                                    ← collapsed header
┃ Allow once   Always allow   Reject   Details
┃  ←→ select · Enter · Esc reject     ← footer band (theme.raised())
```

Expanded variant:

```text
┃ Run shell command  src/main.rs
┃
┃ Execute a shell command and return stdout/stderr.
┃
┃ Arguments
┃ {
┃   "cmd": "cargo test"
┃ }
┃ Allow once   Always allow   Reject   Hide
┃  ←→ select · Enter · Esc reject · ↑↓ scroll details
```

A follow-up **always allow until exit?** confirmation flips the action set
to `Confirm always · Cancel`.

| Key | Effect |
|-----|--------|
| `←` / `→` | Move between action buttons |
| `Enter` | Activate the highlighted action |
| `Esc` | Reject (or cancel the confirm-always step) |
| `↑` / `↓` | Scroll the details body (expanded only) |

The sheet uses a warn-colored left bar (`panel_block(theme.warn(), …)`) as
its severity cue, and `theme.raised()` for the footer band.

## Tool-step detail overlay

Full-output detail overlay for a focused tool step (ADR-0001 step 8). Shows
the step's complete output in a scrollable panel so a long result can be
inspected without scrolling the whole transcript. The largest centered
modal: 92% × 84%.

```text
┃ Run shell command · cargo test              ← brand+bold summary
┃
┃ $ cargo test                                ← bold fg
┃ running 12 tests
┃ test result: ok. 12 passed                  ← fg
┃ warning: unused import                      ← err color
┃ [output truncated]                          ← warn (only if truncated)
┃ ↑/↓ or wheel scroll · esc close             ← muted
```

For non-`Shell` results, the body is just the per-line output in `fg`. See
[Tool step](tool-step.md#detail-overlay) for how the overlay is triggered
and how it relates to inline expansion.

## Help modal

Keybindings cheat sheet (`Ctrl+H`). The narrowest centered modal: 58 × 70.

```text
╭──────────────────────────────────────╮
│ Help                                 │
│                                      │
│ General                              │  ← section header (fg bold)
│ ctrl+p    command palette            │  ← key brand+bold, desc muted
│ enter     send message               │
│ …                                    │
│                                      │
│ Focus zones                          │
│ tab/shift+tab  cycle focus           │
│ ↑↓             walk steps            │
│ esc            back / interrupt      │
│ …                                    │
│                                      │
│ esc · close                          │
╰──────────────────────────────────────╯
```

Sections: **General**, **Line editing**, **Focus zones**, **Views & tools**,
**Modes**. Closes with a one-line note: `Drag to select · Ctrl+C or
Ctrl+Shift+C to copy.`

## Plan preview modal

Read-only preview of the active plan file. Opened by clicking the sticky
plan panel above the input box or pressing `Ctrl+P`. The caller caches the
file content (`App::plan_preview_content`) at open time so the modal does
not hit disk per redraw.

```text
╭────────────────────────────────────────────────────╮
│ Plan preview                                       │
│                                                    │
│ # Goal                                             │
│ - Ship the auth refactor                           │
│ - Step 1: extract session store                    │
│ - Step 2: migrate tests                            │
│ …                                                  │
│                                                    │
│ Esc to close · ↑/↓ scroll · Ctrl+P toggles         │
╰────────────────────────────────────────────────────╯
```

Body text is rendered verbatim (no markdown styling), wrapped at word
boundaries. See [Plan mode](../../explanation/agent-design/plan-mode.md)
for when the plan panel is shown.

## Toasts

Transient top-right notifications rendered above all other chrome. Both
use a 3-row panel via the private `toast` helper, positioned at
`x = term_w − toast_w − 2, y = 1, w = min(text, 58) + 2`, with thick
left+right borders colored by variant.

```text
                                    ┃ press Esc again to interrupt ┃
                                    ┃                              ┃
                                    ┃                              ┃
```

| Toast | Border color | Trigger |
|-------|--------------|---------|
| `draw_armed_toast` | `theme.warn()` | An armed action awaits a second keypress (`Ctrl+C` to exit, `Esc` to interrupt) |
| `draw_copy_toast` (success) | `theme.ok()` | Clipboard write completed |
| `draw_copy_toast` (failure) | `theme.err()` | Clipboard write failed |

## Source

All modals live in `crates/neenee-cli/src/tui/render/overlays.rs`. Shared
primitives (`draw_dim_backdrop`, `centered_rect`, `modal_frame`,
`panel_block`, `toast`) are in `crates/neenee-cli/src/tui/render/primitives.rs`.
The chrome-hiding flag is read by `draw_transcript` in
`crates/neenee-cli/src/tui/render/mod.rs`.
