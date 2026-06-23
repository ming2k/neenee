# Status bar

Transient activity indicator shown directly above the input box.

## Appearance

```text
 ● turn 3 · round 2 · making edits
```

The turn and round segments form a structural prefix (rendered muted) that
anchors the live status. The round segment is omitted before the first model
request of a turn — e.g. during `queued` or `preparing context` — since no
round has started yet:

```text
 ● turn 3 · queued
```

| Attribute | Value |
|-----------|-------|
| Location | 1 row directly above the input box |
| Glyph | `●` (`spinner_glyph`), BOLD |
| Glyph color | `breathing_color(phase, theme.brand(), theme.surface())` — a cosine luminance sweep between brand and surface so the dot breathes at roughly 10 fps instead of cycling braille frames |
| Prefix (`turn N · round M ·`) | `theme.muted()` |
| Status text color | `theme.brand()` + ITALIC |
| Indent | 1 space |

The breathing sweep is the TUI's single liveness anchor — every other
running indicator (tool step, thinking marker, pursuit bar) holds a steady
accent so this dot is the only thing in the user's peripheral vision that
moves. See [ADR-0008](../../adr/0008-single-breathing-anchor.md).

## Visibility

| Condition | Visible? |
|-----------|----------|
| Idle | No |
| Streaming assistant text ("responding") | No — the streamed text is the feedback |
| Running tool / queued / waiting | Yes |
| Overlay modal open | No |

When hidden, the row is returned to the transcript viewport.

## Turn and round

The prefix reports where the agent is in the two-layer execution model. See
[Turns and rounds](../../explanation/agent-design/turns-and-rounds.md) for
the full concept; in short:

| Segment | Meaning |
|---------|---------|
| `turn N` | The user-perceived turn number (1-indexed). Bumped once per submitted message. |
| `round M` | The model-request index within the current turn (1-indexed). Omitted before the first request. A round spans one model request plus the tool work that follows. |

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
driven by `app.spinner_tick` incremented once per frame. Turn and round
values are mirrored from `AgentResponse::RoundStarted` and the harness turn
counter by the response listener in `tui/mod.rs`.
