# TUI reference

The neenee terminal UI is rendered by the in-house
[neenee-tui](../../../crates/neenee-tui/src/lib.rs) engine (ADR-0038): a
retained cell grid with write-marks-dirty tracking, a back/front diff, and a
crossterm backend. The widget layer lives in the
`crates/neenee-code/src/tui/render/` module tree (entry point `render/mod.rs`)
and renders *into* the engine's grid via `Frame::render_widget`.

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

See [layout.md](layout.md) for the footer stack, the sub-agent zoom view,
the modal overlay mode, chrome hiding, and the full measurements table.

## Focus zones

The TUI splits keyboard input into two zones so the same key (arrows, Enter)
has one meaning per zone:

| Zone | Owns keys | How to enter | How to leave |
|------|-----------|--------------|--------------|
| **Compose** (default) | Input box — typing inserts into the prompt | Press any printable key (typically `p`) from Browse | `Ctrl+B` |
| **Browse** | Conversation stream — `↑`/`↓` walk focused steps | `Ctrl+B` from Compose | Any printable key (typically `p`) |

The focus-zone pill (`[ COMPOSE ]` / `[ BROWSE ]`) in the hint bar indicates
which zone is active. `Tab` is completion-only (accepts a slash/path suggestion
when one is open); it is not a zone toggle.

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
| [Sub-agent view](subagent-view.md) | Inline sub-agent step + zoomed-in child stream + navigation bar + focus stack |
| [Activity bar](status-bar.md) | Breathing-dot liveness anchor + turn/round/phase label + pursuit objective + todos progress; clickable to open the Activity modal |
| [Hint bar](hint-line.md) | Focus-zone pill + model/context cluster |
| [Modals](modals.md) | Models, Model editor, Sessions, Session, History, Question, Permission, Tool-step detail, Help, Toasts |

## Other reference

- [Color palette](theme.md) — all `Theme` tokens with RGB values
- [Key measurements](layout.md#key-measurements) — indents, margins, scroll steps
- [Half-block characters](half-block-chars.md) — `╻╹▀▄┃` transition reference

## Source files

| File | Responsibility |
|------|---------------|
| `render/mod.rs` | Draw orchestration: `draw_transcript`, `TranscriptView`, `TranscriptRender`, `transcript_band_rect`, `TRANSCRIPT_H_INSET` |
| `render/design.rs` | Non-color design tokens: spacing, gutters, fixed row counts, text measurement limits |
| `render/theme.rs` | `Theme` (all color tokens) |
| `render/primitives.rs` | `viewport_rect`, `centered_rect`, `panel_block`, `recess_backdrop`, color helpers |
| `render/text_layout.rs` | `wrap_text`, `WrappedLine`, `line_spans`, `code_gutter_line` |
| `render/message_body.rs` | `draw_message_body` (markdown text, user panels, code blocks) |
| `render/step/mod.rs` | Step module: draw orchestration, shared header rendering, sticky-pin tracking |
| `render/step/renderers.rs` | Tool-step, thinking (`draw_reasoning_trace`), and sub-agent step renderers |
| `render/step/state.rs` | Step state machine: `Disclosure`, `Interaction`, summary color/weight computation |
| `render/tools/` | Per-tool-step renderers (one file per tool: `bash`, `edit`, `read`, `grep`, `web`, `ask_user`, `read_image`, `diff`, `meta`, `fallback`) |
| `render/composer.rs` | `draw_composer` (live input box), `INPUT_MSG_IDX` |
| `render/chrome.rs` | `draw_activity_bar` / `ActivityBarHit` (breathing dot + turn/phase + pursuit + todos), `draw_hint_bar` / `HintBarView`, `draw_completion_menu` |
| `render/overlays/` | Modal subsystem (dir): `permission`, `provider`, `session`, `activity`, plus shared `common` and `misc` |
| `render/empty_state.rs` | Empty-transcript placeholder view |
| `render/notice.rs` | Transient notice/toast rendering |
| `render/markdown_table.rs` | `build_table_render`, `shrink_column_widths` |
| `document.rs` | Document model: `TranscriptMessage`, `Block` enum, `MessageKind`, markdown parsing, `parse_arguments_kv` |
| `layout.rs` | `LayoutMap`, `BlockRegion`, `SemanticCursor`, hit-testing |
| `selection.rs` | `SelectionState`, `get_selected_text`, character-boundary snapping |
| `input.rs` | Event-to-action mapping: keyboard, mouse, `InputAction` enum |
| `event_loop.rs` | App loop: state sync, draw orchestration, action handling, `extract_selection_text` |
| `app.rs` | Application state: `App`, `Modal`, `Recess`, activity/session tabs |
| `terminal.rs` | Terminal lifecycle: raw-mode/alt-screen setup-teardown, render-loop wiring |
| `step_interaction.rs` | Browse-zone step focus, toggle, and keyboard interaction |
| `clipboard.rs` / `clipboard_ops.rs` | OSC52 + system clipboard integration; async copy/spawned-ops |
| `completion.rs` | Slash-command / path completion menu |
| `fuzzy.rs` | Fuzzy matcher for history search |
