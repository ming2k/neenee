# Hint line

Single-row status strip below the input box. Left side carries an optional
shell-mode pill; right side carries the model name and context-usage
indicator.

## Appearance

```text
[ COMPOSE ]                        Kimi K2.7 Code   89.2k (8%)
```

In Browse zone the pill switches to `[ BROWSE ]` in the warning tone. When
unattended mode is active, the input box's `›` prompt glyph turns red
(warning tone) instead of its usual brand color — no separate hint-line
badge is shown.

| Attribute | Value |
|-----------|-------|
| Location | 1 row below the input box |
| Zone pill | `[ COMPOSE ]` (brand) / `[ BROWSE ]` (warning) |
| Model name | `brand` + BOLD |
| Context usage | `89.2k` in `text_muted`; `(8%)` in threshold color (green/yellow/red) |
| Background | `surface` |

## Zone switching

| Key | From | To |
|-----|------|-----|
| `Ctrl+B` | Compose | Browse |
| Any printable (typically `p`) | Browse | Compose |

`Tab` is **not** a zone toggle — it only accepts a completion suggestion when
one is open.

## Visibility

Hidden when overlay modals are open.

## Source

`draw_hint_bar` in `render/chrome.rs`.
