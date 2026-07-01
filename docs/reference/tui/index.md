# TUI reference

The neenee terminal UI is split into three layers — see
[architecture.md](architecture.md) for the full picture. In short: the in-house
[neenee-tui](../../../crates/neenee-tui/src/lib.rs) engine (ADR-0038) is a
retained cell grid with write-marks-dirty tracking, a back/front diff, and a
crossterm backend; the **view layer**
([neenee-tui-view](../../../crates/neenee-tui-view/src/lib.rs)) holds the widget
tree (entry point `render/mod.rs`) and the semantic document model, rendering
*into* the engine's grid via `Frame::render_widget`; and the **app shell**
(`crates/neenee-code/src/tui`) owns `App` state, the event loop, and input
mapping, driving the view layer through the borrowed `TranscriptView` seam.

## Frame layout

```text
┌──────────────────────────────────────────────────────────┐
│  Transcript viewport                            (Min 0)  │
│   (messages, expandable steps, sticky pinned summaries)  │
├──────────────────────────────────────────────────────────┤
│  Activity bar                                 (0 or 1 row)  │
│  Input box                         (2 + wrapped lines)  │
│  Hint bar                                       (1 row)  │
└──────────────────────────────────────────────────────────┘
```

See [layout.md](layout.md) for the footer stack, the envoy zoom view,
the modal overlay mode, chrome hiding, and the full measurements table.

## Transcript focus

There are no modal "zones" and no zone-toggle key. Keyboard navigation
rests on a single optional state — the **focused step**
(`App::focused_target`):

| State | Owns keys | How to enter | How to leave |
|-------|-----------|--------------|--------------|
| **Prompt** (default) | Input box — typing inserts into the prompt | (default) | `Ctrl+↑` / `Ctrl+↓` |
| **Focused step** | One transcript step is reverse-highlighted | `Ctrl+↑` / `Ctrl+↓` (nearest step first) | `Esc`, or any printable character falls through to the prompt |

While a step is focused, `↑`/`↓` cycle steps, `Enter` opens it, and the
composer panel drops to its dimmer palette to signal "keys act on the
step." Typing still lands in the prompt. `Tab` is completion-only (accepts
a slash/path suggestion when one is open); it is not a focus toggle.

## Components

| Component | Description |
|-----------|-------------|
| [User message](user-message.md) | Sent prompts on a dimmer panel with `┃` bar |
| [Input box](input-box.md) | Live editable prompt on a brighter panel |
| [Assistant text](assistant-text.md) | Regular markdown text, 4-space indent |
| [Code block](code-block.md) | Borderless code with `┃` bar + line-number gutter |
| [Expandable step](expandable-step.md) | Shared shape for collapsible transcript entries |
| [Tool step](tool-step.md) | Expandable step for tool calls |
| [Thinking step](thinking-step.md) | Expandable step for reasoning text |
| [Step state machine](step-state.md) | The three orthogonal axes (Lifecycle × Disclosure × Interaction) and the accent/weight color channels |
| [Envoy view](envoy-view.md) | Inline envoy step + zoomed-in child stream + navigation bar + focus stack |
| [Activity bar](status-bar.md) | Breathing-dot liveness anchor + live status label + pursuit objective + todos progress + elapsed; clickable to open the Activity modal |
| [Hint bar](hint-line.md) | Optional `[ SHELL ]` pill + model/context cluster |
| [Modals](modals.md) | Models, Model editor, Sessions, Session, History, Question, Permission, Tool-step detail, Help, Toasts |

## Other reference

- [Architecture](architecture.md) — the engine / view / shell layers, the
  `TranscriptView` seam, and the component reuse tiers
- [Color palette](theme.md) — all `Theme` tokens with RGB values
- [Key measurements](layout.md#key-measurements) — indents, margins, scroll steps
- [Half-block characters](half-block-chars.md) — `╻╹▀▄┃` transition reference

## Source files

See [architecture.md](architecture.md) for how these three groups depend on each
other. Paths below are relative to each crate's `src/`.

### View layer — `crates/neenee-tui-view/src/`

| File | Responsibility |
|------|---------------|
| `render/mod.rs` | Draw orchestration: `draw_transcript`, `TranscriptView`, `TranscriptRender`, `transcript_band_rect`, `TRANSCRIPT_H_INSET` |
| `render/design.rs` | Non-color design tokens: spacing, gutters, fixed row counts, text measurement limits |
| `render/theme.rs` | `Theme` (all color tokens) |
| `render/primitives.rs` | `viewport_rect`, `centered_rect`, `panel_block`, `recess_backdrop`, `modal_area`, color helpers |
| `render/text_layout.rs` | `wrap_text`, `WrappedLine`, `line_spans`, `code_gutter_line` |
| `render/message_body.rs` | `draw_message_body` (markdown text, user panels, code blocks) |
| `render/disclosure/mod.rs` | Disclosure module: draw orchestration, shared header rendering, sticky-pin tracking |
| `render/disclosure/renderers.rs` | Tool-step, thinking (`draw_reasoning_trace`), and envoy step renderers |
| `render/disclosure/state.rs` | Step state machine: `Disclosure`, `Interaction`, summary color/weight computation |
| `render/layout/` | Transcript arrangement strategies: `compact`, `turn_band` (selected by `[tui] transcript_layout`) |
| `render/tools/` | Per-tool-step renderers (one file per tool: `bash`, `edit`, `read`, `grep`, `web`, `ask_user`, `read_image`, `diff`, `meta`, `fallback`) |
| `render/composer.rs` | `draw_composer` (live input box), `INPUT_MSG_IDX` |
| `render/chrome.rs` | `draw_activity_bar` / `ActivityBarHit` (breathing dot + round/phase + pursuit + todos), `draw_hint_bar` / `HintBarView`, `draw_completion_menu` |
| `render/overlays/` | Modal subsystem (dir): one renderer per modal — `permission`, `provider`, `history`, `help`, `session`, `permissions_manager`, `activity`, `config`, `config_layout`, `config_nudge`, `mcp`, `skills`, `tools`, `token_report`, `toast` — plus shared `common` helpers |
| `render/empty_state.rs` | Empty-transcript placeholder view; `parse_logo` |
| `render/notice.rs` | Transient notice/toast rendering |
| `render/markdown_table.rs` | `build_table_render`, `shrink_column_widths` |
| `document.rs` | Document model: `TranscriptMessage`, `Block` enum, `MessageKind`, markdown parsing, `parse_arguments_kv` |
| `layout.rs` | `LayoutMap`, `BlockRegion`, `SemanticCursor`, hit-testing |
| `selection.rs` | `SelectionState`, `get_selected_text`, character-boundary snapping |
| `fuzzy.rs` | Fuzzy matcher for history / provider search |
| `providers.rs` | Provider/model picker ranking + display helpers (`model_display_name`, `RankedProvider`, …) |
| `modal.rs` | Shared discriminants: `Modal`, `Recess`, `ActivityTab` |
| `completion.rs` | Completion-menu data types: `Completion`, `CompletionKind` (matching logic stays in the shell) |

### App shell — `crates/neenee-code/src/tui/`

| File | Responsibility |
|------|---------------|
| `mod.rs` | Entry point `run_tui`; re-exports the view modules at their `crate::tui::*` paths |
| `app.rs` | Application state: `App`, `CaretOwner`, scroll/zoom snapshots |
| `event_loop.rs` | App loop: state sync, draw orchestration, action handling, `extract_selection_text` |
| `input/` | Event-to-action mapping (dir): `mod.rs` keyboard/mouse dispatch, `InputAction` enum, `tests.rs` |
| `terminal.rs` | Terminal lifecycle: raw-mode/alt-screen setup-teardown, render-loop wiring |
| `step_interaction.rs` | Transcript-step focus, toggle, and keyboard interaction |
| `clipboard.rs` / `clipboard_ops.rs` | OSC52 + system clipboard integration; async copy/spawned-ops |
| `completion.rs` | Slash-command / `@path` completion **logic** (`impl App`); reuses the view layer's data types |
| `question_model.rs` | Question-modal state machine |
