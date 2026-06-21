# 0008. Single breathing anchor for TUI liveness

- **Status:** Accepted
- **Date:** 2026-06-22

## Context

ADR-0001 introduced a "breathing dot" — a slow cosine luminance sweep — as
the TUI's liveness indicator and applied it broadly: the activity bar above
the input box, every `Running` tool-step summary, the `●` marker on a
streaming reasoning trace, and (when ADR-0007's goal bar landed) the dot on
the goal bar. Four call sites of `breathing_color` shared one primitive.

After living with the design for a few weeks, the per-step breathing turned
into a liability rather than a feature:

- A transcript with three concurrent tool calls and a streaming reasoning
  trace breathes in unison at five separate places. The dots do not phase
  (they share one `spinner_tick`), so the whole transcript pulses as one
  mass. That reads as visual noise, not as localized liveness.
- The activity bar — which is the *only* place a "is the harness alive?"
  glance should land — loses its role as the peripheral anchor. With four
  other dots competing for the same peripheral slot, the user can no longer
  "look at the transcript and notice in the corner of the eye that
  something is still running." They have to scan.
- A `Running` tool step or a streaming reasoning trace routinely lasts tens
  of seconds (provider round trips, slow bash). A 1.2 s breathing loop with
  no progress signal leaves the user unsure whether the call is progressing
  or stuck — exactly the failure mode NN/g's progress-indicator research
  warns about for indeterminate spinners held past ~10 s.

The design-theory grounding for changing this:

- **Von Restorff effect (isolation effect).** A stimulus that differs from
  its surroundings is the one that gets attended to. The more components
  breathe, the less any one of them is the isolate — the anchor dissolves.
- **Visual hierarchy.** Motion is the highest-contrast tool a designer has
  (dynamic vs. static). Spending it on every running surface pushes every
  running surface to the top of the hierarchy simultaneously, which is the
  same as having no hierarchy.
- **Miller's law (7 ± 2).** Concurrent animated elements are working-memory
  load. One motion anchor is free; five is a tax.
- **Jakob's Law.** The user's mental model from other TUIs (claude-code,
  cargo, npm) is "one spinner per running thing." neenee deliberately chose
  a quieter aesthetic — but the right way to express that aesthetic is *one*
  quiet anchor, not five of them.

## Decision

Concentrate the entire motion budget in **one** place: the activity-bar
dot. Every other running indicator switches to a steady accent and conveys
lifecycle through hue and glyph alone.

| Component | Before | After |
|-----------|--------|-------|
| Activity bar (`draw_activity_bar`) | Breathing dot, brand sweep | **Unchanged** — the single breathing anchor |
| Goal bar (`draw_goal_bar`) | Breathing dot, brand sweep | Steady `theme.brand()` dot; progress carried by the `[done/total]` suffix |
| Tool step `Running` (`draw_tool_step`) | Summary text breathing-swept between `info` and `surface` | Steady `theme.info()` accent (same hue, no sweep) |
| Reasoning marker (`draw_reasoning_trace`) | `●` marker breathing while streaming | Steady `theme.info()` `●` while streaming, `+`/`-` marker once done |

The `breathing_color` function stays in `render/chrome.rs` as the
activity-bar primitive; it just loses its other three call sites.
`spinner_phase` is dropped from `GoalBarView`, `draw_tool_step`, and
`draw_reasoning_trace` signatures (the activity bar still consumes it via
`TranscriptView::spinner_phase`).

The three-channel split that replaces per-step breathing:

| Channel | Carries | Example |
|---------|---------|---------|
| **Motion** (activity bar only) | "The harness is alive" | Breathing `●` |
| **Hue** (per step) | Lifecycle category | `info` running / `error_fg` failed / `warn` denied / `text_muted` cancelled |
| **Glyph** (per step) | Lifecycle state within a kind | `●` while a trace streams, `+`/`-` once it finishes; `[done/total]` for goal progress |

## Alternatives considered

- **Keep per-step breathing, slow it down.** A 3 s cycle instead of 1.2 s
  reads as calmer but does not solve the Von Restorff problem — five
  in-phase slow pulses are still five anchors competing, just slower.
- **Phase-offset the per-step sweeps.** Mechanically possible (each step
  gets `spinner_tick + mi` as its phase) but the result reads as random
  shimmer, not as localized liveness. Also increases cognitive load: the
  user cannot tell which dot belongs to which step.
- **Replace breathing with braille spinners per step.** This is the
  claude-code / cargo convention. Rejected because it abandons the
  "quiet dot" aesthetic ADR-0001 chose and because braille frames read as
  "computing" rather than "waiting," which is the wrong metaphor for a
  provider round trip.
- **Keep per-step breathing on the reasoning marker only (one extra
  anchor).** Rejected — the reasoning marker is the worst candidate for
  breathing because a thinking trace is the longest-running surface in the
  TUI (often 30 s+) and therefore the most likely to trigger the
  "is this stuck?" anxiety that an indeterminate loop produces past 10 s.
- **Drop the activity-bar breathing too and use only steady colors.**
  Rejected — without any motion at all, a multi-second provider wait reads
  as a frozen terminal, which is the exact failure ADR-0001 introduced the
  breathing dot to fix.

## Consequences

Positive:

- The activity-bar dot is restored as the single peripheral anchor. One
  glance answers "is the harness alive?" without scanning the transcript.
- A transcript full of running steps no longer pulses; the user can read
  finished content next to in-flight content without the in-flight rows
  jittering their luminance.
- The lifecycle of a step is still unambiguous: hue says *which* lifecycle
  (`info` = running, `error_fg` = failed, etc.), and the activity-bar dot
  says *the harness is still working on it*. Neither channel has to do the
  other's job.
- Smaller render signatures: `spinner_phase` is gone from
  `draw_tool_step`, `draw_reasoning_trace`, and `GoalBarView`. New render
  paths (e.g. a future sticky-banner or overlay) do not have to thread a
  spinner phase just to opt in to breathing.

Negative:

- A collapsed `Running` tool step is now visually indistinguishable from a
  collapsed `Denied` step at a glance, except by hue (`info` vs. `warn`).
  Users who run neenee on a monochrome terminal or with red-green color
  blindness lose the secondary motion cue. Mitigation: the activity-bar
  dot still says "harness is busy," and the per-step `+`/`-`/`●` marker
  still distinguishes the disclosure / streaming axis; only the
  within-running-failed-denied distinction narrows to hue alone.
- A long-running tool step no longer has any per-step motion, so a user
  staring at one step (rather than the activity bar) cannot tell from
  that row alone whether it is progressing or hung. Mitigation: streaming
  tool bodies (`bash` stdout, structured `Shell`) still update in place
  under the header, and the activity bar carries the global liveness
  signal.

Migration:

- No persistence or schema change. The `spinner_tick` field on `App` and
  the `spinner_phase` field on `TranscriptView` stay — the activity bar
  still consumes them.
- Snapshot tests in `render/snapshot_tests.rs` are unaffected because
  they capture text + background only, never foreground color. The
  breathing sweep was foreground-only, so its removal changes no
  snapshot.

## References

- [ADR-0001](0001-tool-rendering-redesign.md) — introduced `breathing_color`
  and the per-step `Running` accent that this ADR narrows back to one call
  site.
- [TUI explanation → A breathing dot, not a spinner](../explanation/tui.md#a-breathing-dot-not-a-spinner)
  — user-facing rationale for the single-anchor rule.
- [Step state machine](../reference/tui/step-state.md) — the hue / weight
  channels that carry per-step lifecycle in the steady-accent world.
- Von Restorff, H. (1933), *Über die Wirkung von Bereichsbildungen im
  Spurenfeld*; Wikipedia's [Von Restorff effect](https://en.wikipedia.org/wiki/Von_Restorff_effect)
  summary.
- Sherwin, K. (2014), *Progress Indicators Make a Slow System Less
  Insufferable*, NN/g — the 2–10 s window for indeterminate spinners and
  the "looks stuck" failure mode past it.
