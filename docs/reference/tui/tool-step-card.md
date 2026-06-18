# Tool-step card

A concrete [expandable card](expandable-card.md) for tool calls (read_file,
bash, edit_file, etc.). The header summarizes the call; the expanded body
shows the technical name, arguments, and result.

## Collapsed

```text
  + Read crates/main.rs · 0ms
```

| Attribute | Value |
|-----------|-------|
| Background | `element_bg` (33, 37, 54) band, inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD |
| Marker color | Status-colored (`success` / `error_fg` / `info`) |
| Header text | Human-readable description + duration, `text_muted` BOLD |
| Header text column | 4 from transcript edge (band col 2, after `+ ` prefix) |

The header shows only what the tool did and how long it took. The technical
tool name is inside the expanded body.

## Bash collapsed preview

The `bash` tool is the one tool where the command and its output are useful
to glance at without expanding. So its collapsed state shows a truncated
preview under the header instead of the header alone. Other tool types keep
the header-only collapsed form.

```text
  + Run npm test · 1.2s

  $ npm test
  PASS  src/foo.rs
  PASS  src/bar.rs
  …
```

| Attribute | Value |
|-----------|-------|
| Background | `menu_bg` (27, 30, 44) — recessed from the `element_bg` header |
| Body indent | 2 cols (same as the expanded body, aligns with the header text) |
| Command line | `$ ` + first line of the command, `text` color |
| Output lines | First 8 lines (`BASH_PREVIEW_LINES`), ANSI-stripped, `text_muted` |
| Truncation marker | `…` in `dim_fg`, shown only when output exceeds 8 lines |
| Lines truncated to | Inner width (hard cut, no per-line ellipsis) |

Clicking any preview row focuses and toggles the card open (the whole preview
is registered as part of the header's hit region). Expanding swaps the preview
for the full structured body (Tool / Arguments / Result) — the preview is only
for the collapsed glance.

## Expanded

```text
  - Read crates/main.rs · 0ms

   Tool
     read_file

   Arguments
     path: crates/main.rs
     limit: 50

   Result
     1  fn main() {
     2      ...
```

A blank `menu_bg` row separates the header from the body and every pair
of sections (Tool / Arguments / Result / children) so each part breathes.

### Body sections

| Section | Content | Background |
|---------|---------|------------|
| Tool | Technical tool name in `dim_fg` | `menu_bg` |
| Arguments | Compact `key: value` pairs in `text_muted` | `menu_bg` |
| Result | Tool-specific rendering (see below) | `code_bg` for content; label on `menu_bg` |

- Section labels ("Tool", "Arguments", "Result") in `text_muted` + BOLD,
  indented 1 col. All three labels share the `menu_bg` background so they
  align vertically; only the Result *content* sits on the recessed `code_bg`.
- Section content indented 2 cols so it left-aligns with the header text.
  The Result code gutter also starts at col 2, so code rows align with the
  surrounding body content.
- Arguments parsed from JSON into key-value pairs via `parse_arguments_kv`,
  not displayed as a raw JSON code block.

### Result rendering (per tool)

The Result section dispatches on the tool name so structured output gets a
purpose-built renderer instead of a generic code block:

| Tool | Renderer | Notes |
|------|----------|-------|
| `list_dir`, `glob` | `render_listing_content` | One entry per row, no gutter. Directories (entries ending in `/`) in `info`, files in `code_fg`. |
| `grep` | `render_grep_content` | Matches grouped under a bold `heading_fg` file-path header; each match shown as `{lineno}  {content}` with the line-number column aligned and dimmed. |
| `bash` | `render_bash_content` | Plain wrapped rows, no gutter (line numbers are meaningless for command output). Section markers emitted by the tool (`Exit N`, `STDOUT:`, `STDERR:`, `(success, stderr):`, `[Output truncated`, `[Output was large`) are highlighted in `warning`. |
| `read_file`, `edit_file`, others | `render_code_content` | Code block with line-number gutter on `code_bg` (the original behavior). Used as the fallback for unrecognized tools. |

### Status colors

| State | Color | Header suffix |
|-------|-------|---------------|
| Completed | `success` (green) | ` · 0ms` |
| Failed | `error_fg` (red) | ` · failed 0ms` |
| Running | `info` (cyan) | (no suffix) |

## Interaction

See [expandable card](expandable-card.md#behavior) for the shared toggle,
sticky-pin, and narrow-fallback behavior. Tool-step specifics:

- `Ctrl+T` expands or collapses all tool-step cards.
- `Tab` / `Shift+Tab` includes visible tool-step cards in the keyboard focus
  order; `Enter` / `Space` activates the focused card.

## Sub-agent children

Nested sub-task tool calls render as indented child cards inside the parent's
expanded body (6-space indent). Child cards show a compact `⚒` header line.

## Source

`render_tool_step_card` and `render_tool_body_section` in `render.rs`. Shared
header via `render_expandable_card_header`. Bash collapsed preview via
`render_bash_preview` (with `strip_ansi` and `BASH_PREVIEW_LINES`). Result
rendering dispatched by `render_tool_result_section` to
`render_listing_content`, `render_grep_content`, `render_bash_content`, or
`render_code_content`. Header data from `tool_step_header()` and
`parse_arguments_kv()` in `document.rs`.
