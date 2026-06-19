# Tool-step card

A concrete [expandable card](expandable-card.md) for tool calls (read_file,
bash, edit_file, etc.). The header summarizes the call; the expanded body
shows the arguments and a structured result. Results are typed
[`ToolOutput`](../../adr/0001-tool-rendering-redesign.md) (Shell/Code/Listing/
Matches/…), so each tool renders from data instead of a sniffed string;
`bash` streams stdout live into its collapsed preview while it runs.

## Collapsed

```text
  + Read crates/main.rs · 0ms
```

| Attribute | Value |
|-----------|-------|
| Background | `element_bg` (21, 23, 22) band, inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD |
| Status indicator | Conveyed by header color only — no glyph. Breathing accent while running (luminance sweep), `error_fg` on failure, `text_muted` when cancelled, neutral (`text`/`text_muted` by focus) on success |
| Header text | Human-readable description + duration, BOLD |

The header shows only the marker, the summary, and the duration — no status
glyph and no per-tool icon. Run state reads purely from the header color, so a
successful call stays calm (neutral text) while in-flight, failed, and
cancelled calls pick up an accent color. The technical tool name is inside the
expanded body.

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
| Background | `menu_bg` (17, 19, 18) — recessed from the `element_bg` header |
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
| `list_dir`, `glob` | `draw_listing_content` | One entry per row, no gutter. Directories (entries ending in `/`) in `info`, files in `code_fg`. |
| `grep` | `draw_grep_content` | Matches grouped under a bold `heading_fg` file-path header; each match shown as `{lineno}  {content}` with the line-number column aligned and dimmed. |
| `bash` | `draw_bash_content` | Renders from the structured `Shell` payload: stdout lines, then stderr in `error_fg`, then an `exit N` / `[output truncated]` footer. No string-sniffing of `Exit`/`STDERR:` markers. While running, the collapsed preview streams stdout live. |
| `edit_file`, `write_file` | `draw_diff_content` | A real `similar`-based unified diff: line-number gutter, `+`/`-` sign column, and intra-line word highlight on the changed spans. |
| `read_file`, others | `draw_code_content` | Code block with line-number gutter on `code_bg` (the fallback for unrecognized tools). |

### Status colors

Status is conveyed by the header text color (there is no status glyph).

| State | Header color | Header suffix |
|-------|--------------|---------------|
| Completed | neutral (`text` focused / `text_muted` otherwise) | ` · 0ms` |
| Failed | `error_fg` (red) | ` · failed 0ms` |
| Running | breathing `info` accent (luminance sweep) | (no suffix) |
| Cancelled | `text_muted` | (no suffix) |

Success stays neutral so the common case reads as calm; only in-flight, failed,
and cancelled calls pick up an accent. (The child-step accents and sticky-pin
color still use the raw status palette — `success`/`error_fg`/`info`/`text_muted`
— derived from [`ToolStatus::color`].)

## Detail overlay

`Enter` on a focused tool-step card opens a centered, scrollable panel showing
the step's complete output — the full structured payload, not the
transcript-truncated view. For `Shell` it renders `$ command`, stdout, stderr
(in `error_fg`), and the exit/truncation footer directly from the
`ToolOutput::Shell` fields. `↑`/`↓`/wheel scrolls; `Esc`/`Enter` closes.
Sub-agent `task` cards still navigate into the child session on `Enter`
instead of opening the overlay. The bulk `Ctrl+T` toggle still
inline-expands every card for those who want the old all-expanded view. See
[ADR-0001](../../adr/0001-tool-rendering-redesign.md).

## Interaction

See [expandable card](expandable-card.md#behavior) for the shared toggle,
sticky-pin, and narrow-fallback behavior. Tool-step specifics:

- `Enter` on a focused tool-step opens the [detail overlay](#detail-overlay)
  (clicking the header toggles it inline).
- `Ctrl+T` expands or collapses all tool-step cards.
- `Tab` / `Shift+Tab` includes visible tool-step cards in the keyboard focus
  order.

## Sub-agent children

Nested sub-task tool calls render as indented child cards inside the parent's
expanded body (6-space indent). Child cards show a compact one-line header
(the summary, colored by run state) with no marker glyph.

## Source

`draw_tool_step_card` and `draw_tool_body_section` in
`crates/neenee-tui/src/render/turn_artifacts.rs`. Shared header via
`draw_expandable_card_header`. Bash collapsed/expanded preview via the
`BashPresenter`/`draw_bash_content` (with `strip_ansi` and
`BASH_PREVIEW_LINES`). Result rendering dispatched by
`draw_tool_result_section` to `draw_listing_content`,
`draw_grep_content`, `draw_bash_content`, `draw_diff_content`, or
`draw_code_content`. The structured payload comes from `ToolOutput`
([ADR-0001](../../adr/0001-tool-rendering-redesign.md)); header data from
`tool_step_header()` and `parse_arguments_kv()` in `document.rs`. The
detail overlay is `draw_tool_step_detail_overlay` in
`crates/neenee-tui/src/render/overlays.rs`.
