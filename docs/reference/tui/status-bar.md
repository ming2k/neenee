# Status bar

Transient activity indicator shown directly above the input box.

## Appearance

```text
 ⠹ making edits
```

| Attribute | Value |
|-----------|-------|
| Location | 1 row directly above the input box |
| Spinner | Braille: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` cycling at 100 ms tick rate |
| Text color | `accent` + ITALIC |
| Indent | 1 space |

## Visibility

| Condition | Visible? |
|-----------|----------|
| Idle | No |
| Streaming assistant text ("responding") | No — the streamed text is the feedback |
| Running tool / queued / waiting | Yes |
| Overlay modal open | No |

When hidden, the row is returned to the transcript viewport.

## Activity labels

| Tool / phase | Label |
|--------------|-------|
| Queued | `queued` |
| Waiting for provider | `waiting for model` |
| `read_file` / `list_dir` / `use_skill` | `exploring` |
| `grep` | `searching codebase` |
| `write_file` / `edit_file` | `making edits` |
| `bash` | `running command` |
| `goal_checklist` | `updating tasks` |
| MCP tools (`mcp__*`) | `using MCP` |
| Finalizing stream | `finalizing response` |
| Autonomous loop | `loop 2/8 · making edits` |

## Source

`draw_status_bar` in `render.rs`. Spinner phase driven by `app.spinner_tick`
incremented once per frame in `lib.rs`.
