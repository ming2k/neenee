# Goal bar

Transient goal indicator shown directly above the status bar (and thus above
the input box). Only visible while an active, incomplete goal is set.

## Appearance

```text
 ● ship the goal bar feature  [1/2]
```

| Attribute | Value |
|-----------|-------|
| Location | 1 row above the status bar, below the plan panel |
| Glyph | `●` (`spinner_glyph`), BOLD |
| Color | `theme.brand()` (steady — the bar's existence on the `raised` background signals an active goal; progress is carried by the `[done/total]` suffix) |
| Objective | `theme.muted()`, truncated to `GOAL_OBJECTIVE_MAX_CHARS = 28` chars with `...` suffix |
| Progress | `[done/total]` appended when the checklist is non-empty |
| Background | `raised` (entire row, so it reads as a clickable surface) |

## Interaction

Clicking anywhere on the goal bar triggers `/goal status`, which surfaces the
full goal detail (objective, completion state, and checklist) in the transcript.

## Visibility

| Condition | Visible? |
|-----------|----------|
| No goal set | No |
| Active, incomplete goal | Yes |
| Completed goal | No |
| Overlay modal open | No |
| Sub-agent zoom view | No |

When hidden, the row is returned to the transcript viewport.

## Source

`draw_goal_bar` / `GoalBarView` in `render/chrome.rs`. Checklist summary from
`goal_checklist_summary` in the same module. Goal model in
`neenee_core::goals::Goal`.
