# Input box

The live editable prompt at the bottom of the frame.

## Appearance

```text
  ╻▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀     ← top transition
  ┃ type here…                        ← text row(s)
  ╹▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄     ← bottom transition
```

| Attribute | Value |
|-----------|-------|
| Background | `panel_bg` (22, 24, 35) — brighter = "editable" |
| Left/right margin | 2 cols of `app_bg` |
| Accent bar | `┃` in `accent` (Build mode) or Plan-mode blue |
| Text color | `text` (brighter than sent messages) |
| Text indent | 4 cols (2 margin + `┃` + 1 leading space) |
| Top/bottom padding | Half-block transition rows (see [half-block-chars](half-block-chars.md)) |

## Height growth

The box grows with wrapped content, capped at half the terminal height so the
transcript history always stays visible. The layout reserves space based on
`wrap_text(input, text_width).len()`.

## Caret

Blinking terminal caret positioned on the active wrapped line. Clamped to the
visible inner area when the input is very long.

## Selection

Semantic mouse-drag selection works on input text via `INPUT_MSG_IDX`
(`usize::MAX - 2`) in the layout map. Copy extracts from `app.input` using
byte-precise ranges. Layout recording is skipped when the API-key modal masks
the display.

## Visibility

Hidden when overlay modals are open (see [modals](modals.md)). The composer
input is also borrowed by the [model editor](modals.md#model-editor) and
the [history search](modals.md#history-search-modal) modals — they route
keystrokes through the same surface but render their own framing around it.

## Source

`draw_composer` in `render/composer.rs`. Rendered manually (not via a `Block`
widget) so the `┃` bar can be half-height (`╻`/`╹`) on transition rows. `INPUT_MSG_IDX = usize::MAX - 2` is the layout-map
message index reserved for live input selection.
