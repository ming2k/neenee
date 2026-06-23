# Pursuit bar

Transient pursuit indicator shown directly above the status bar (and thus above
the input box). Only visible while an active, incomplete pursuit is set.

## Appearance

```text
 ● ship the pursuit bar feature  [1/2]
```

| Attribute | Value |
|-----------|-------|
| Location | 1 row above the status bar, below the plan panel |
| Glyph | `●` (`spinner_glyph`), BOLD |
| Color | `theme.brand()` (steady — the bar's existence on the `raised` background signals an active pursuit; progress is carried by the `[done/total]` suffix) |
| Objective | `theme.muted()`, truncated to `GOAL_OBJECTIVE_MAX_CHARS = 28` chars with `...` suffix |
| Progress | `[done/total]` appended when the checklist is non-empty |
| Background | `raised` (entire row, so it reads as a clickable surface) |

## Interaction

Clicking anywhere on the pursuit bar triggers `/pursuit status`, which surfaces the
full pursuit detail (objective, completion state, and checklist) in the transcript.

## Visibility

| Condition | Visible? |
|-----------|----------|
| No pursuit set | No |
| Active, incomplete pursuit | Yes |
| Completed pursuit | No |
| Overlay modal open | No |
| Sub-agent zoom view | No |

When hidden, the row is returned to the transcript viewport.

## Source

`draw_pursuit_bar` / `PursuitBarView` in `render/chrome.rs`. Checklist summary from
`goal_checklist_summary` in the same module. Pursuit model in
`neenee_core::pursuits::Pursuit`.
