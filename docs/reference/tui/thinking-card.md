# Thinking card

A concrete [expandable card](expandable-card.md) for model reasoning /
chain-of-thought text.

## Collapsed

```text
  + Thinking · 1.2s · 140 chars
```

| Attribute | Value |
|-----------|-------|
| Background | `element_bg` (21, 23, 22) band, inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD, `info` color |
| Header text | `text_muted` BOLD |
| Header text column | 4 from transcript edge (band col 2, after `+ ` prefix) |

## Header format

| State | Format |
|-------|--------|
| Streaming | `Thinking · {chars} chars` (duration omitted) |
| Completed | `Thinking · {duration} · {chars} chars` |

## Expanded

```text
  - Thinking · 1.2s · 140 chars

    reasoning text in text_muted...
```

A blank `menu_bg` row separates the header from the body; consecutive text
blocks are likewise blank-separated. Paragraph breaks inside a single block
are already preserved as empty rows by `wrap_text`.

| Attribute | Value |
|-----------|-------|
| Background | `menu_bg` (17, 19, 18) |
| Body indent | 2 cols inside the band (transcript column 4, left-aligned with the header text) |
| Body color | `text_muted` |
| Body style | Plain wrapped text (no code gutter) |

## Interaction

See [expandable card](expandable-card.md#behavior) for the shared toggle,
sticky-pin, and narrow-fallback behavior.

Thinking cards participate in the same keyboard focus order as tool-step
cards. `Enter` / `Space` opens or closes the focused thinking card.

## Source

`render_thinking_card` in `render.rs`. Shared header via
`render_expandable_card_header`. Header data from `thinking_header()` in
`document.rs`.
