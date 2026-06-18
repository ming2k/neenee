# Header

The header (HUD bar) is the top-most element of the frame.

## Appearance

```text
 gpt-4o
──────────────────────────────────────────
```

- Model name in `accent` + BOLD, 1-space left indent.
- Optional goal appended: `   ◎ objective… [2/5]` in `text_muted`.
- Optional checklist dock (second row): ` Tasks 2/5  current task`.
- Thin `border_subtle` bottom rule via `RtBlock::borders(BOTTOM)`.

## What it does not show

No brand name, no logo dot, no provider name, no mode badge. The model name
alone is the visual anchor.

## Height

| Condition | Height |
|-----------|--------|
| Normal (no checklist) | 2 rows (content + separator) |
| With checklist | 3 rows (content + checklist + separator) |
| Overlay modal open | 0 rows (hidden) |

## Source

`draw_chat` in `render.rs` — header spans built inline, rendered through a
`RtBlock` with `Borders::BOTTOM`.
