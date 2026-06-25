# Color palette

All colors are defined in `Theme::default()` (`crates/neenee-cli/src/tui/render/theme.rs`).

## Backgrounds

| Token | RGB | Purpose |
|-------|-----|---------|
| `app_bg` | (7, 8, 8) | Darkest base; fills the entire frame |
| `backdrop` | (3, 4, 4) | Dim overlay behind modals (darker than `app_bg`) |
| `code_bg` | (13, 14, 14) | Code blocks and tool-step results |
| `user_panel_bg` | (11, 12, 12) | Sent user-message band (dimmer than input) |
| `panel_bg` | (14, 15, 15) | Input box + header panel (brighter = "editable" / chrome) |
| `menu_bg` | (17, 19, 18) | Suggestion / completion menus |
| `user_bg` | (18, 24, 21) | Tinted band behind the user's own messages |
| `element_bg` | (21, 23, 22) | Footer / option bars |
| `selected_bg` | (38, 48, 44) | Semantic-selection highlight |

## Foregrounds

| Token | RGB | Purpose |
|-------|-----|---------|
| `text` | (213, 213, 205) | Primary text (input box, selected) |
| `text_muted` | (119, 125, 117) | Sent messages, labels, secondary text |
| `assistant_fg` | (213, 213, 205) | Assistant text |
| `user_fg` | (165, 177, 164) | User message text |
| `system_fg` | (111, 116, 110) | System / harness messages |
| `code_fg` | (166, 178, 163) | Code content |
| `heading_fg` | (190, 194, 181) | Markdown headings |
| `quote_fg` | (156, 145, 118) | Blockquotes |
| `dim_fg` | (94, 99, 94) | Line-number gutter, tool name |
| `primary` | (142, 161, 145) | Brand / selection; hint-line keys; `┃` bars; breathing-dot indicator |
| `success` | (117, 148, 117) | Completed tool status; context-usage indicator < 70% |
| `info` | (128, 153, 156) | Running tool status, thinking marker |
| `warning` | (181, 149, 93) | Warnings; context-usage indicator 70–90% |
| `error_fg` | (190, 111, 104) | Failed tool status; context-usage indicator > 90% |

## Modifiers

| Field | Default | Purpose |
|-------|---------|---------|
| `modal_dim_factor` | `0.5` | Brightness multiplier (0.0–1.0) applied to every cell of the live surface while a **Dim**-recess modal is open. The terminal cannot alpha-blend, so a dim-recess modal darkens the transcript/chrome in place by scaling each color by this factor — lower is darker. See [Modals](modals.md). |

## Background hierarchy

```text
backdrop (3,4,4)        ← dimmest; modal overlay
app_bg (7,8,8)          ← base; entire frame
  code_bg (13,14,14)        ← code blocks
  user_panel_bg (11,12,12)  ← sent messages (dimmer = read-only)
  panel_bg (14,15,15)       ← input box + header panel (chrome)
  menu_bg (17,19,18)        ← menus / suggestion popups
  user_bg (18,24,21)        ← user-message tint
  element_bg (21,23,22)     ← footer / option bars (brightest panel)
selected_bg (38,48,44)  ← selection highlight
```

The header is a floating half-block panel on `panel_bg` (same as the input
box), inset from the edges by `app_bg` gutters; no separator rules are drawn.
