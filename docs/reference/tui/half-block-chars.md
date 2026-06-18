# Half-block transition characters

Used on the top and bottom padding rows of user messages and the input box to
create a half-row visual inset instead of a full empty row.

## Character reference

| Character | Unicode | Name | Half filled |
|-----------|---------|------|-------------|
| `┃` | U+2503 | BOX DRAWINGS HEAVY VERTICAL | Full height |
| `╻` | U+257B | BOX DRAWINGS HEAVY DOWN | Bottom half only |
| `╹` | U+2579 | BOX DRAWINGS HEAVY UP | Top half only |
| `▀` | U+2580 | UPPER HALF BLOCK | Top half = fg color |
| `▄` | U+2584 | LOWER HALF BLOCK | Bottom half = fg color |

## Transition logic

### Top transition row

The panel "fades in" from the bottom half, connecting to the text row below.

| Column | Character | fg | bg | Effect |
|--------|-----------|----|----|--------|
| Bar | `╻` | `accent` | `app_bg` | Bottom-half bar visible |
| Content | `▄` | panel bg | `app_bg` | Bottom half = panel color |

### Bottom transition row

The panel "fades out" from the top half, connecting from the text row above.

| Column | Character | fg | bg | Effect |
|--------|-----------|----|----|--------|
| Bar | `╹` | `accent` | `app_bg` | Top-half bar visible |
| Content | `▀` | panel bg | `app_bg` | Top half = panel color |

## Visual result

```text
  ╻▄▄▄▄▄▄▄▄▄▄▄▄▄▄     ← top: only bottom half carries panel + bar
  ┃ text content          ← full height
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀     ← bottom: only top half carries panel + bar
```

The `┃` bar and panel background smoothly fade in/out in half-cell increments.
