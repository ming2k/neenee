# Activity bar

Transient activity indicator shown directly above the input box. It unifies
the live status label, the active pursuit, todos progress, and the
breathing-dot liveness anchor into one click-to-open bar.

## Appearance

```text
 ● making edits · ⟴ refactor auth module · todos 2/5 · 23s
```

The bar surfaces what the user most wants to know mid-round: the **live
status** (lead, brand + italic), an optional **pursuit badge** (`⟴ <objective>`,
shown only while a pursuit is armed), **todos progress** (`todos d/t`,
shown only when a non-empty task list exists), and **elapsed** time (the only
live counter). Segments are omitted when there is nothing to report, so a
plain round reads simply:

```text
 ● making edits · 3s
```

The structural counters — `turn N · round M · <model>` — no longer live on
the bar. They take space and change rarely, so they moved into the
**Activity modal** that this bar opens on click. The whole bar is a click
target (and `Tab`/`Enter` opens the modal): one glance answers "what's
happening, are there todos, how long?", one click shows the full breakdown
(Activity tab: pursuit, current prompt, turn/round/model/elapsed; Tasks tab:
the todo list).

| Attribute | Value |
|-----------|-------|
| Location | 1 row directly above the input box |
| Glyph | `●` (`spinner_glyph`), BOLD |
| Glyph color | `breathing_color(phase, theme.brand(), theme.surface())` — a cosine luminance sweep between brand and surface so the dot breathes at roughly 10 fps instead of cycling braille frames |
| Status text color | `theme.brand()` + ITALIC |
| Pursuit / todos / elapsed | `theme.muted()` |
| Indent | 1 space |

The breathing sweep is the TUI's single liveness anchor — every other
running indicator (tool step, thinking marker) holds a steady
accent so this dot is the only thing in the user's peripheral vision that
moves. See [ADR-0008](../../adr/0008-single-breathing-anchor.md).

## Visibility

| Condition | Visible? |
|-----------|----------|
| Idle | No |
| Streaming assistant text ("responding") | Yes — the bar stays up across the whole round lifecycle, sustaining the breathing-dot liveness anchor (ADR-0008) through the longest phase |
| Running tool / queued / waiting | Yes |
| Overlay modal open | No |

The bar persists from round start (user submits) through every phase —
`queued`, `responding`, tool work, `finalizing response` — and only
disappears when the harness returns to idle. This keeps the breathing dot
in peripheral vision for the entire active round and avoids a layout shift
at the streaming boundary.

## Turn and round

The bar no longer shows the turn/round counters; they live in the Activity
modal (click the bar) as a detail line `turn N · round M · <model> ·
<elapsed>`. See [Rounds and turns](../../explanation/agent-design/rounds-and-turns.md)
for the full concept; in short:

| Counter | Meaning |
|---------|---------|
| `turn N` | The user-perceived turn number (1-indexed). Bumped once per submitted message. |
| `round M` | The model-request index within the current turn (1-indexed). A round spans one model request plus the tool work that follows. |

The round number resets each turn; the turn number resets only on a new
session.

## Activity labels

| Tool / phase | Label |
|--------------|-------|
| Queued | `queued` |
| Waiting for provider | `waiting for model` |
| `read_file` / `list_dir` / `use_skill` | `exploring` |
| `grep` | `searching codebase` |
| `write_file` / `edit_file` | `making edits` |
| `bash` | `running command` |
| MCP tools (`mcp__*`) | `using MCP` |
| Finalizing stream | `finalizing response` |
| Autonomous loop | `loop 2/8` prefix ahead of the activity |
| Provider retry | `retry 1/4 in 3s · <reason>` — the reason tail is the truncated error message |

## Source

`draw_activity_bar` in `render/chrome.rs`. Glyph from `spinner_glyph`;
luminance sweep from `breathing_color` in the same module. Spinner phase
driven by `app.spinner_tick` incremented once per frame. Round and turn
values are mirrored from `AgentResponse::RoundStarted` and the harness round
counter by the response listener in `tui/mod.rs`.
