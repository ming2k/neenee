# Thinking card

Expandable card for model reasoning / chain-of-thought text.

## Collapsed

```text
  ▶ Thinking · 1.2s · 140 chars
```

| Attribute | Value |
|-----------|-------|
| Background | `element_bg` (33, 37, 54) full-width band |
| Arrow | `▶` / `▼`, BOLD, `info` color |
| Header text | `text_muted` BOLD |
| Indent | 2 cols |

## Header format

| State | Format |
|-------|--------|
| Streaming | `Thinking · {chars} chars` (duration omitted) |
| Completed | `Thinking · {duration} · {chars} chars` |

## Expanded

```text
  ▼ Thinking · 1.2s · 140 chars
   reasoning text in assistant_fg...
```

| Attribute | Value |
|-----------|-------|
| Background | `menu_bg` (27, 30, 44) |
| Body indent | 3 cols |
| Body color | `assistant_fg` |
| Body style | Plain wrapped text (no code gutter) |

## Interaction

- Click header or press `Enter` to toggle.
- Same sticky-header behavior as tool-step cards.

## Source

`render_thinking_card` in `render.rs`. Header data from `thinking_header()` in
`document.rs`.
