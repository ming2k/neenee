# Hint line

Right-aligned keybinding hints below the input box.

## Appearance

```text
                            ctrl+p commands   ctrl+h help   enter send
```

| Attribute | Value |
|-----------|-------|
| Location | 1 row below the input box |
| Alignment | Right-aligned |
| Key color | `primary` (34, 211, 238) + BOLD |
| Description color | `text_muted` |
| Separator | 3 spaces between entries |

## Visibility

Hidden when overlay modals are open.

## Source

`draw_hint` in `render.rs`.
