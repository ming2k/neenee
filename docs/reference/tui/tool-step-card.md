# Tool-step card

Expandable card for tool calls (read_file, bash, edit_file, etc.).

## Collapsed

```text
  ▶ Read crates/main.rs · 0ms
```

| Attribute | Value |
|-----------|-------|
| Background | `element_bg` (33, 37, 54) full-width band |
| Arrow | `▶` (collapsed) / `▼` (expanded), BOLD |
| Arrow color | Status-colored (`success` / `error_fg` / `info`) |
| Header text | Human-readable description + duration, `text_muted` BOLD |
| Indent | 2 cols |

The header shows only what the tool did and how long it took. The technical
tool name is inside the expanded body.

## Expanded

```text
  ▼ Read crates/main.rs · 0ms
 Tool
   read_file
 Arguments
   path: crates/main.rs
   limit: 50

 Result
 1  fn main() {
 2      ...
```

### Body sections

| Section | Content | Background |
|---------|---------|------------|
| Tool | Technical tool name in `dim_fg` | `menu_bg` |
| Arguments | Compact `key: value` pairs in `text_muted` | `menu_bg` |
| Result | Output as code block with line-number gutter | `code_bg` |

- Section labels ("Tool", "Arguments", "Result") in `text_muted` + BOLD.
- Blank separator line between Arguments and Result.
- Arguments parsed from JSON into key-value pairs via `parse_arguments_kv`, not
  displayed as a raw JSON code block.
- Result rendered with line-number gutter using `code_gutter_line`.

### Status colors

| State | Color | Header suffix |
|-------|-------|---------------|
| Completed | `success` (green) | ` · 0ms` |
| Failed | `error_fg` (red) | ` · failed 0ms` |
| Running | `info` (cyan) | (no suffix) |

## Interaction

- Click header or press `Enter` on a selected card to toggle expand/collapse.
- `Ctrl+T` expands or collapses all tool-step cards.
- Sticky header: when an expanded card's body scrolls past the top of the
  viewport, its header pins under the HUD bar.

## Sub-agent children

Nested sub-task tool calls render as indented child cards inside the parent's
expanded body (6-space indent). Child cards show a compact `⚒` header line.

## Source

`render_tool_step_card` and `render_tool_body_section` in `render.rs`.
Header data from `tool_step_header()` and `parse_arguments_kv()` in `document.rs`.
