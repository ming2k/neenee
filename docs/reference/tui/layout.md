# Frame layout

## Vertical chunks

The terminal frame is divided into three vertical chunks by ratatui's `Layout`:

| Chunk | Height | Contents |
|-------|--------|----------|
| Header | 0 (modal) / 2 / 3 (checklist) | Model name, goal, thin separator |
| Chat | `Min(0)` | All message content |
| Bottom | 0 (modal) / `status + input + 2` | Status bar, input box, hint line |

The entire frame is first painted with `app_bg` so the TUI owns every pixel.

## Chrome hiding

When an overlay modal is open, `chrome_hidden = true` collapses the header and
bottom heights to 0. The modal gets the full terminal area with no header, input
box, status bar, or hint line visible.

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
| Left/right margin (input, user messages) | 2 cols | `app_bg` gutters |
| `┃` bar column | 2 (after 2-col margin) | User messages, code blocks, input |
| Assistant text indent | 4 cols | `line_spans("    ", ...)` |
| Code block indent | 2 cols + `┃` + space | `code_gutter_line(left_indent=2)` |
| Card header indent | 2 cols | `card_header_line("  {} ", arrow)` |
| Card body indent | 3 cols (labels 1) | `render_tool_body_section` |
| Line-number gutter min width | 2 chars | `.max(2)` |
| Mouse scroll step | 4 rows | `ScrollUp/Down` handler |
| PageUp/PageDown step | `view_height - 1` | One line of overlap |
| Input box max height | `terminal_height / 2` | Capped so chat stays visible |
| Message spacing | 1 row | Between consecutive messages |
