# Color palette

All colors are defined in `Theme::default()` (`crates/neenee-tui/src/render.rs`).

## Backgrounds

| Token | RGB | Purpose |
|-------|-----|---------|
| `app_bg` | (15, 16, 25) | Darkest base; fills the entire frame |
| `user_panel_bg` | (18, 20, 30) | Sent user-message band (dimmer than input) |
| `panel_bg` | (22, 24, 35) | Input box (brighter = "editable") |
| `code_bg` | (22, 24, 35) | Code blocks and tool-step results |
| `menu_bg` | (27, 30, 44) | Tool-step / thinking card body |
| `element_bg` | (33, 37, 54) | Card headers, sticky headers |
| `backdrop` | (8, 9, 14) | Dim overlay behind modals |
| `selected_bg` | (30, 50, 70) | Semantic-selection highlight |

## Foregrounds

| Token | RGB | Purpose |
|-------|-----|---------|
| `text` | (205, 214, 244) | Primary text (input box, selected) |
| `text_muted` | (122, 132, 153) | Sent messages, labels, secondary text |
| `assistant_fg` | (205, 214, 244) | Assistant text |
| `code_fg` | (148, 226, 213) | Code content |
| `dim_fg` | (127, 132, 156) | Line-number gutter, tool name |
| `accent` | (94, 234, 212) | `┃` bars, model name, spinners |
| `success` | (74, 222, 128) | Completed tool status |
| `error_fg` | (243, 139, 168) | Failed tool status |
| `info` | (125, 211, 252) | Running tool status, thinking arrow |
| `border_subtle` | (45, 50, 70) | Header separator rule |
| `primary` | (34, 211, 238) | Hint-line keys |
| `warning` | (250, 204, 21) | Warnings |
| `heading_fg` | (94, 234, 212) | Markdown headings |
| `quote_fg` | (249, 226, 175) | Blockquotes |

## Background hierarchy

```
app_bg (15,16,25)       ← darkest, entire frame
  user_panel_bg (18,20,30)  ← sent messages (dimmer = read-only)
  panel_bg (22,24,35)       ← input box (brighter = editable)
  code_bg (22,24,35)        ← code blocks
menu_bg (27,30,44)      ← card bodies
element_bg (33,37,54)   ← card headers (brightest panel)
```
