# Frame layout

## Vertical chunks

The terminal frame is divided into three vertical chunks by ratatui's `Layout`:

| Chunk | Height | Contents |
|-------|--------|----------|
| Header | 0 (modal) / 2 / 3 (checklist) | Model name, goal, thin separator |
| Chat | `Min(0)` | All message content |
| Footer | 0 (modal) / `status + input + 2` | Status bar, input box, hint line |

The entire frame is first painted with `app_bg` so the TUI owns every pixel.

## Chrome hiding

When an overlay modal is open, `chrome_hidden = true` collapses the header and
footer heights to 0. The modal gets the full terminal area with no header,
input box, status bar, or hint line visible.

Modal types that hide chrome: Models, Sessions, Help, Permission.
Modal types that keep chrome: None, ApiKey, Endpoint, ModelName, HistorySearch.

## Chat viewport behavior

- Messages render top-to-bottom with 1-row spacing between them.
- Auto-follow pins to the newest content.
- Scrolling up pauses follow; scrolling back to the bottom (or sending a message)
  re-engages it.

## Key measurements

| Measurement | Value | Where |
|------------|-------|-------|
| Left/right gutter (all chat content) | 2 cols `app_bg` | `CHAT_H_INSET`, applied via `chat_band_rect` (cards) / explicit spans (user panel, code block) / wrap-width slack (markdown) |
| `┃` bar column | 2 (after 2-col gutter) | User messages, code blocks, input |
| Assistant text indent | 4 cols (left) + 2-col right gutter | `line_spans("    ", ...)`; wraps at `area.width - 6` |
| Code block indent | 2 cols (inside band) + `┃` + space | `code_gutter_line(left_indent=2)` |
| Card marker column | 2 (inside `CHAT_H_INSET` band) | `+` / `-` at band col 0 in `card_header_line` |
| Card header text column | 4 (2 gutter + 2 after `+ `) | After `+ ` prefix |
| Card body indent | 4 cols from chat edge (2 inside band) | `render_tool_body_section`, `render_thinking_card` |
| Line-number gutter min width | 2 chars | `.max(2)` |
| Mouse scroll step | 4 rows | `ScrollUp/Down` handler |
| PageUp/PageDown step | `view_height - 1` | One line of overlap |
| Input box max height | `terminal_height / 2` | Capped so chat stays visible |
| Message spacing | 1 row | Between consecutive messages |
