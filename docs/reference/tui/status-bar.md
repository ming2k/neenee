# Status bar

Transient activity indicator shown directly above the input box.

## Appearance

```text
 ● making edits
```

| Attribute | Value |
|-----------|-------|
| Location | 1 row directly above the input box |
| Glyph | `●` (`spinner_glyph`), BOLD |
| Color | `breathing_color(phase, theme.brand(), theme.surface())` — a cosine luminance sweep between brand and surface so the dot breathes at roughly 10 fps instead of cycling braille frames |
| Text color | `theme.brand()` + ITALIC |
| Indent | 1 space |

The breathing sweep is the same primitive per-step `Running` accents use,
so a running tool step and the status bar share one notion of "alive".

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

`draw_status_bar` in `render/chrome.rs`. Glyph from `spinner_glyph`;
luminance sweep from `breathing_color` in the same module. Spinner phase
driven by `app.spinner_tick` incremented once per frame in `lib.rs`.
