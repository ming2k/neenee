# TUI reference

The neenee terminal UI is built with [ratatui] and rendered in
`crates/neenee-tui/src/render.rs`.

## Frame layout

```
┌──────────────────────────────────────────────────────────┐
│  Header                                            (0–3)  │
├──────────────────────────────────────────────────────────┤
│  Chat viewport                                  (Min 0)  │
├──────────────────────────────────────────────────────────┤
│  Status bar                                (0 or 1 row)  │
│  Input box                         (2 + wrapped lines)  │
│  Hint line                                       (1 row)  │
└──────────────────────────────────────────────────────────┘
```

See [layout.md](layout.md) for chunk heights, chrome hiding, and the full
measurements table.

## Components

| Component | Description |
|-----------|-------------|
| [Header](header.md) | Model name + optional goal, thin separator rule |
| [User message](user-message.md) | Sent prompts on a dimmer panel with `┃` bar |
| [Input box](input-box.md) | Live editable prompt on a brighter panel |
| [Assistant text](assistant-text.md) | Regular markdown text, 4-space indent |
| [Code block](code-block.md) | Borderless code with `┃` bar + line-number gutter |
| [Tool-step card](tool-step-card.md) | Expandable card for tool calls |
| [Thinking card](thinking-card.md) | Expandable card for reasoning text |
| [Status bar](status-bar.md) | Animated braille spinner + activity label |
| [Hint line](hint-line.md) | Right-aligned keybinding hints |
| [Modals](modals.md) | Models, Sessions, History, Permission, API key, Help |

## Other reference

- [Color palette](theme.md) — all `Theme` tokens with RGB values
- [Key measurements](layout.md#key-measurements) — indents, margins, scroll steps
- [Half-block characters](half-block-chars.md) — `╻╹▀▄┃` transition reference

## Source files

| File | Responsibility |
|------|---------------|
| `render.rs` | All rendering: `draw_chat`, `draw_input`, `draw_status_bar`, `draw_hint`, card renderers, modal renderers, `Theme`, `ChatView` |
| `document.rs` | Document model: `ChatMessage`, `Block` enum, `MessageKind`, markdown parsing, `parse_arguments_kv` |
| `layout.rs` | `LayoutMap`, `BlockRegion`, `SemanticCursor`, hit-testing |
| `selection.rs` | `SelectionState`, `get_selected_text`, character-boundary snapping |
| `input.rs` | Event-to-action mapping: keyboard, mouse, `InputAction` enum |
| `lib.rs` | App loop: state sync, draw orchestration, action handling, `extract_selection_text` |
| `clipboard.rs` | OSC52 + system clipboard integration |

[ratatui]: https://ratatui.rs
