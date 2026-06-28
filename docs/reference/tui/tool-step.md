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
| Status indicator | Conveyed by header color only — no glyph. Resolved through the shared [step state machine](step-state.md): `Ok` falls through to the disclosure × interaction weight ladder; `Running` / `Failed` / `Denied` / `Cancelled` each supply a steady accent (`info` / `error_fg` / `warn` / `text_muted`) that wins outright |
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
| `bash` | `draw_bash_content` | A `$ command` prompt line, then the captured lines in **arrival order** — stdout and stderr interleaved exactly as the process wrote them, each coloured by source stream (stderr in `error_fg`) — then an `exit N` / `[output truncated]` footer, all one `code_bg` block. Carriage returns are collapsed (only the text after the last `\r` on a line survives). The ordered view comes from the structured `Shell::lines` field (available while streaming); legacy/restored payloads with only flat `stdout`/`stderr` fall back to the all-stdout-then-all-stderr bands. Command comes from the structured `Shell` payload, falling back to the parsed arguments. |
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
The full hue / luminance resolution is centralized in the
[step state machine](step-state.md); the tool-step-specific suffix on the
summary line is:

| State | Header suffix |
|-------|---------------|
| Completed | ` · 0ms` |
| Failed | ` · failed 0ms` |
| Running | (no suffix) |
| Cancelled | (no suffix) |

(The child-step accents and sticky-pin color use the raw
[`ToolStatus::color`](step-state.md#lifecycle-accent) palette directly. Per
[ADR-0008](../../adr/0008-single-breathing-anchor.md), the activity bar is the
single breathing anchor, so the parent summary carries a steady accent while
running — no luminance sweep.)

## Inline disclosure

Activating a focused tool step — `Enter`, a click on its summary, or a
right-click — toggles its inline disclosure, expanding the body in place to
show the full structured payload (not the transcript-truncated view). For
`Shell` the expanded body renders `$ command`, the captured lines in
**arrival order** (stdout and stderr interleaved as written, stderr in
`error_fg`), and the exit/truncation footer directly from the
`ToolOutput::Shell` fields. Sub-agent `subagent` steps navigate into the
child session on `Enter`/click instead of expanding. The bulk `Ctrl+T`
toggle expands or collapses every step at once. See
[ADR-0001](../../adr/0001-tool-rendering-redesign.md).

## Interaction

See [expandable step](expandable-step.md#behavior) for the shared toggle,
sticky-pin, and narrow-fallback behavior. Tool-step specifics:

- `Enter` on a focused tool step toggles its inline disclosure — the same
  effect as clicking its summary or right-clicking it.
- `Ctrl+T` expands or collapses all tool steps.
- `↑` / `↓` while a step is focused includes visible tool steps in the keyboard focus order.

## Sub-agent children

Nested sub-task tool calls render as indented child steps inside the parent's
expanded body (6-space indent), flat on `app_bg`. Each child shows a compact
one-line header (the summary, colored by run state) with no marker glyph.

## Source

`draw_tool_step` in `crates/neenee-code/src/tui/render/step/renderers.rs`. Shared
header via `draw_expandable_step_header` (from
`crates/neenee-code/src/tui/render/step/mod.rs`). Expanded content dispatched by
`draw_tool_result` to `draw_listing_content`, `draw_grep_content`,
`draw_bash_content` (which renders the `$ command` line + the structured
`Shell` payload), `draw_diff_content`, or `draw_code_content`. The bash command
is resolved by `bash_command_for`. Presenters (summary / `result_kind` /
`arg_layout`) live in `crates/neenee-code/src/tui/render/tools/`. The structured
payload comes from `ToolOutput`
([ADR-0001](../../adr/0001-tool-rendering-redesign.md)); header data from
`tool_step_header()` and `parse_arguments_kv()` in `document.rs`.
