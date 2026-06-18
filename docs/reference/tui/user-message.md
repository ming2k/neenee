# User message

Sent user prompts displayed in the chat transcript.

## Appearance

```text
  ╻▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄     ← top transition
  ┃ typed message text here          ← text row
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀     ← bottom transition
```

| Attribute | Value |
|-----------|-------|
| Background | `user_panel_bg` (18, 20, 30) — dimmer than input |
| Left/right margin | 2 cols of `app_bg` |
| Accent bar | `┃` in `accent` at column 2 |
| Text color | `text_muted` — signals "read-only, already sent" |
| Text indent | 4 cols (2 margin + `┃` + 1 space) |
| Top/bottom padding | Half-block transition rows (see [half-block-chars](half-block-chars.md)) |

## Selection

Character-level semantic selection — only the dragged substring gets
`selected_bg`, not the whole line. Copy returns the display text verbatim.

## Contrast with input box

| Property | User message | Input box |
|----------|-------------|-----------|
| Background | `user_panel_bg` (dimmer) | `panel_bg` (brighter) |
| Text color | `text_muted` | `text` |
| Editable | No | Yes |

## Source

`render_message_blocks` → `Block::Text` with `is_user == true` in `render.rs`.
