# Tool step

An [expandable step](expandable-step.md) for a tool call (`read_file`, `bash`,
`edit_file`, …). It renders flat on the app background — no band, no section
labels — like a [thinking step](thinking-step.md). The header alone summarizes
the call (tool + key arguments + duration); expanding reveals the tool-specific
content directly. Results are typed
[`ToolOutput`](../../adr/0001-tool-rendering-redesign.md) (Shell/Code/Listing/
Matches/Patch), so each tool renders from structured data instead of a sniffed
string.

## Collapsed

The default state. Header only — no preview, no body. The whole point of
collapsing is to keep noisy tool I/O out of the transcript until you ask for it.

```text
  + Read crates/main.rs · 0ms
```

| Attribute | Value |
|-----------|-------|
| Background | `app_bg` (flat — no band), inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD |
| Status indicator | Conveyed by header color only — no glyph. Breathing accent while running (luminance sweep), `error_fg` on failure, `text_muted` when cancelled, neutral (`text`/`text_muted` by focus) on success |
| Header text | Human-readable description + duration, BOLD |

## Expanded

Flat on `app_bg`: a blank row, then the tool-specific content indented 2 cols
(to align with the header text), then a blank row. There are **no** `Tool` /
`Arguments` / `Result` labels and no surrounding `menu_bg` band — the content
speaks for itself. Only the content block carries a `code_bg` so it reads as a
distinct panel against the app background.

```text
  - Read crates/main.rs · 0ms

    1  fn main() {
    2      ...
```

### Content rendering (per tool)

Dispatch is by `result_kind`, so structured output gets a purpose-built
renderer instead of a generic code block. `bash` additionally prefixes its
block with a `$ command` line, so an expanded bash step reads like a terminal
session.

| Tool | Renderer | Notes |
|------|----------|-------|
| `bash` | `draw_bash_content` | A `$ command` prompt line, then stdout, then stderr in `error_fg`, then an `exit N` / `[output truncated]` footer — all one `code_bg` block. Command comes from the structured `Shell` payload (available while streaming), falling back to the parsed arguments. |
| `list_dir`, `glob` | `draw_listing_content` | One entry per row, no gutter, on `code_bg`. Directories (entries ending in `/`) in `info`, files in `code_fg`. |
| `grep` | `draw_grep_content` | Matches grouped under a bold `heading_fg` file-path header; each match shown as `{lineno}  {content}` with the line-number column aligned and dimmed. |
| `edit_file`, `write_file` | `draw_diff_content` | A real `similar`-based unified diff: line-number gutter, `+`/`-` sign column, and intra-line word highlight on the changed spans, on `code_bg`. |
| `read_file`, others | `draw_code_content` | Code block with line-number gutter on `code_bg` (the fallback for unrecognized tools). |

Unknown / MCP tools (`arg_layout = KeyValue`) print their arguments as plain
`key: value` rows on `app_bg` before the result block, since the header only
carries the primary argument. The key names are self-describing, so no label is
needed; the result block's `code_bg` keeps the two visually distinct.

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

`Enter` on a focused tool step opens a centered, scrollable panel showing the
step's complete output — the full structured payload, not the
transcript-truncated view. For `Shell` it renders `$ command`, stdout, stderr
(in `error_fg`), and the exit/truncation footer directly from the
`ToolOutput::Shell` fields. `↑`/`↓`/wheel scrolls; `Esc`/`Enter` closes.
Sub-agent `task` steps still navigate into the child session on `Enter`
instead of opening the overlay. The bulk `Ctrl+T` toggle still inline-expands
every step for those who want the old all-expanded view. See
[ADR-0001](../../adr/0001-tool-rendering-redesign.md).

## Interaction

See [expandable step](expandable-step.md#behavior) for the shared toggle,
sticky-pin, and narrow-fallback behavior. Tool-step specifics:

- `Enter` on a focused tool step opens the [detail overlay](#detail-overlay)
  (clicking the header toggles it inline).
- `Ctrl+T` expands or collapses all tool steps.
- `↑` / `↓` in Browse zone includes visible tool steps in the keyboard focus order.

## Sub-agent children

Nested sub-task tool calls render as indented child steps inside the parent's
expanded body (6-space indent), flat on `app_bg`. Each child shows a compact
one-line header (the summary, colored by run state) with no marker glyph.

## Source

`draw_tool_step` in `crates/neenee-cli/src/tui/render/step/renderers.rs`. Shared
header via `draw_expandable_step_header` (from
`crates/neenee-cli/src/tui/render/step/mod.rs`). Expanded content dispatched by
`draw_tool_result` to `draw_listing_content`, `draw_grep_content`,
`draw_bash_content` (which renders the `$ command` line + the structured
`Shell` payload), `draw_diff_content`, or `draw_code_content`. The bash command
is resolved by `bash_command_for`. Presenters (summary / `result_kind` /
`arg_layout`) live in `crates/neenee-cli/src/tui/render/tools/`. The structured
payload comes from `ToolOutput`
([ADR-0001](../../adr/0001-tool-rendering-redesign.md)); header data from
`tool_step_header()` and `parse_arguments_kv()` in `document.rs`. The detail
overlay is `draw_tool_step_detail_overlay` in
`crates/neenee-cli/src/tui/render/overlays.rs`.
