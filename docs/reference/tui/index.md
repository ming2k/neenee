# TUI reference

The neenee terminal UI is built with [ratatui] and rendered by the
`crates/neenee-tui/src/render/` module tree (entry point `render/mod.rs`).

## Frame layout

```text
┌──────────────────────────────────────────────────────────┐
│  Header                                            (0–4)  │
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
| [Header](header.md) | Floating `panel_bg` half-block panel: model name + goal + context-usage bar |
| [User message](user-message.md) | Sent prompts on a dimmer panel with `┃` bar |
| [Input box](input-box.md) | Live editable prompt on a brighter panel |
| [Assistant text](assistant-text.md) | Regular markdown text, 4-space indent |
| [Code block](code-block.md) | Borderless code with `┃` bar + line-number gutter |
| [Expandable card](expandable-card.md) | Shared shape for collapsible cards |
| [Tool-step card](tool-step-card.md) | Expandable card for tool calls |
| [Thinking card](thinking-card.md) | Expandable card for reasoning text |
| [Status bar](status-bar.md) | Animated braille spinner + activity label |
| [Hint line](hint-line.md) | Right-aligned keybinding hints |
| [Sidebar](layout.md#sidebar-column) | Right-side persistent pane (goal, plans, loop) |
| [Modals](modals.md) | Models, Sessions, History, Permission, API key, Help |

## Other reference

- [Color palette](theme.md) — all `Theme` tokens with RGB values
- [Key measurements](layout.md#key-measurements) — indents, margins, scroll steps
- [Half-block characters](half-block-chars.md) — `╻╹▀▄┃` transition reference

## Source files

| File | Responsibility |
|------|---------------|
| `render/mod.rs` | Draw orchestration: `draw_chat`, `ChatView`, `ChatRender`, `chat_band_rect`, `CHAT_H_INSET` |
| `render/theme.rs` | `Theme` (all color tokens) |
| `render/primitives.rs` | `viewport_rect`, `centered_rect`, `panel_block`, `draw_dim_backdrop`, color helpers |
| `render/text_layout.rs` | `wrap_text`, `WrappedLine`, `line_spans`, `code_gutter_line` |
| `render/message_body.rs` | `draw_message_body` (markdown text, user panels, code blocks) |
| `render/turn_artifacts.rs` | Tool-step, thinking, and sub-agent cards; sticky header; sub-agent bar |
| `render/composer.rs` | `draw_composer` (live input box), `INPUT_MSG_IDX` |
| `render/chrome.rs` | `draw_status_bar`, `draw_hint`, `draw_suggestions`, `spinner_frame` |
| `render/sidebar.rs` | `draw_sidebar`, `SidebarView`, `SIDEBAR_WIDTH`, `SIDEBAR_AUTO_WIDTH` |
| `render/overlays.rs` | Modals: models, sessions, history, permission, API key, help, solution |
| `render/markdown_table.rs` | `build_table_render`, `shrink_column_widths` |
| `document.rs` | Document model: `ChatMessage`, `Block` enum, `MessageKind`, markdown parsing, `parse_arguments_kv` |
| `layout.rs` | `LayoutMap`, `BlockRegion`, `SemanticCursor`, hit-testing |
| `selection.rs` | `SelectionState`, `get_selected_text`, character-boundary snapping |
| `input.rs` | Event-to-action mapping: keyboard, mouse, `InputAction` enum |
| `lib.rs` | App loop: state sync, draw orchestration, action handling, `extract_selection_text` |
| `clipboard.rs` | OSC52 + system clipboard integration |

[ratatui]: https://ratatui.rs
