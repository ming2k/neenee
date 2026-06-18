# Assistant text

Regular markdown prose from the assistant model.

## Appearance

```text
    This is a paragraph of assistant text that
    wraps across multiple lines.
```

| Attribute | Value |
|-----------|-------|
| Indent | 4 spaces from the left edge |
| Right gutter | 2 cols of `app_bg` (`CHAT_H_INSET`); wraps at `area.width - 6` |
| Color | `assistant_fg` (205, 214, 244) |
| Background | `app_bg` (transparent, no panel band) |
| Wrapping | CJK kinsoku rules — closing punctuation does not begin a line |

## Selection

Character-level via `line_spans`. The 4-space prefix is part of the prefix
span, so hit-testing maps correctly to byte offsets within the block content.

## Contrast with other text types

| Type | Indent | Background |
|------|--------|------------|
| Assistant text | 4 cols | `app_bg` (transparent) |
| User message | 4 cols (2 margin + `┃` + space) | `user_panel_bg` |
| Code block | 4 cols (2 indent + `┃` + space) | `code_bg` |

## Source

`render_message_blocks` → `Block::Text` with `is_user == false` in `render.rs`.
