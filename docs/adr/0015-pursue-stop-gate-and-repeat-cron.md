# 0015. Pursue stop-gate and repeat cron scheduler (replace `/goal` + `/loop`)

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

ADR-0010 slimmed the goal primitive to `{objective, is_complete, checklist}`
and ADR-0009 made the autonomous `/loop` uncapped. Two commands served
autonomous work:

- `/goal <objective>` set a persisted objective injected into the **system
  prompt** every turn, gated by an in-memory **checklist**.
- `/loop` ran an **outer multi-turn loop**, re-entering the transcript each
  iteration with a hidden control prompt until the model emitted the
  completion marker.

Benchmarking against Claude Code (closed-source; reverse-engineered from the
installed `2.1.186` binary) revealed a cleaner split it had already adopted:

- Its `/goal` is a **Stop hook**: a condition registered on the
  turn-stop event that blocks the turn from ending until the condition holds,
  re-injecting the condition on each stop attempt. The autonomous "pursuit"
  is *within-turn* continuation, not an outer loop of whole turns. The
  condition lives in a **user-role** directive, not the system prompt.
- Its `/loop` is a **cron scheduler** that repeats an arbitrary prompt on a
  time interval — a completely separate, clock-driven concern.

neenee conflated both: `/goal` was passive system-prompt context, `/loop` was
the real driver, and the checklist added a second completion gate that rarely
earned its complexity. The two commands' meanings were also unintuitive
(`/loop` was a timer elsewhere; here it was a goal driver).

## Decision

Replace `/goal` + `/loop` with two mechanism-distinct commands, each mirroring
one Claude-Code subsystem:

1. **`/pursue <condition>` — a stop-gate (condition-driven, within-turn).**
   Setting a pursuit persists the condition, **arms a stop-gate** on the
   `Agent`, and drives a single agent turn. At each turn-loop exit (mirroring
   the existing verify-nudge gate, in both the streaming and non-streaming
   loops), `Agent::pursuit_continuation` checks: if a pursuit is armed, an
   active (incomplete) goal exists, the latest response did not signal
   completion, and a safety cap (`MAX_PURSUIT_ITERATIONS = 50`) is not
   exhausted, it re-injects the condition as a hidden user message and forces
   another round instead of ending the turn. Completion is the model's signal
   (the `[NEENEE_GOAL_COMPLETE]` marker) — the gate *gates*, the model
   *signals* (matching Claude Code: no separate LLM judge). `/pursue` subsumes
   the old `/loop`; there is no outer multi-turn loop.

   This cap does **not** reintroduce the per-turn round cap ADR-0009 removed.
   ADR-0009 keeps an *ordinary* turn (no pursuit armed) uncapped: it ends when
   the model stops calling tools. `MAX_PURSUIT_ITERATIONS` only bites when a
   user has explicitly armed a stop-gate that overrides that natural stop by
   re-injecting a hidden prompt — i.e. it bounds the *forced re-injection*, not
   the model's own tool-calling. Without it an armed pursuit whose condition the
   model never signals would loop indefinitely with no human in the gate.

2. **`/repeat <cron> <prompt>` — a cron scheduler (clock-driven, recurring).**
   A real five-field cron expression engine (`neenee_core::cron::CronExpr`)
   computes fire times. Jobs are durable in SQLite (`repeat.db`,
   `neenee_core::RepeatStore`), auto-expire after 30 days, and a background
   scheduler (`start_repeat_scheduler`) ticks every 30 s, firing due jobs as
   normal `AgentRequest::Chat` turns. This is orthogonal to pursuits.

Supporting changes:

- **Drop the checklist** primitive entirely (`GoalChecklistItem`,
  `GoalChecklistStatus`, `can_complete`, the `goal_checklist` tool, the
  completion-defer gate). Completion is now a single boolean driven by the
  marker.
- **Move the condition out of the system-prompt's role as the driver.** The
  objective is still surfaced in the system prompt for visibility, but the
  *driving force* is the stop-gate's per-stop re-injection (user-role hidden
  message), not the prompt.
- Rename `start_goal_loop`→`start_pursuit`, `LoopRunContext`→`PursuitContext`,
  `LoopCheckpoint`→`PursuitCheckpoint`. Remove the now-dead
  `/loop resume` path (`discard_trailing_loop_prompts`,
  `LoopCheckpoint::resume_iteration`).
- Keep the `[NEENEE_GOAL_COMPLETE]` marker and `goals.db` schema (back-compat).

## Alternatives considered

- **Keep `/loop` as an outer multi-turn loop alongside the stop-gate.**
  Rejected: redundant. The stop-gate makes a single turn run to completion, so
  an outer loop would either double-drive or, on safety-cap exhaustion
  (`Ok(false)`), hang re-running disarmed turns. One driver is correct.
- **Separate LLM-as-judge for "condition met" on each stop.** Rejected: no
  such judge exists in Claude Code's binary (verified), and it would add a
  model call per stop for no fidelity gain. The working model signals
  completion; the harness gates.
- **Keep the checklist as an optional completion gate.** Rejected: it was the
  main source of complexity and rarely changed completion outcomes; the
  boolean + marker compose cleanly.
- **Full cron vs. simple `Nm/Nh/Nd` interval parser.** Chose full cron: more
  expressive, matches Claude Code's scheduler, and the engine is small and
  fully tested.

## Consequences

- **Positive:** one intuitive command per driving dimension (`/pursue` =
  "until condition", `/repeat` = "on a clock"); within-turn pursuit avoids the
  per-turn prompt overhead of the old outer loop; the checklist surface area
  is gone; `/repeat` jobs survive restarts.
- **Negative:** breaking change — `/goal` and `/loop` no longer exist; any
  user muscle memory or scripts must migrate to `/pursue` (`/loop resume` has
  no equivalent; a pursuit is single-turn). `/pursue` is bounded by a 50-round
  safety cap (configurable later) rather than truly uncapped.
- **Neutral:** `PursuitCheckpoint` retains the legacy `{iteration,
  max_iterations}` fields for snapshot back-compat even though a pursuit is a
  single turn.

## References

- [ADR-0009](0009-uncapped-agentic-loop.md) — uncapped agentic loop (history).
- [ADR-0010](0010-slim-goal-primitive.md) — slimming the goal primitive.
- Claude Code `2.1.186` binary (closed-source) — reverse-engineered Stop-hook
  `/goal` and cron `/loop` for reference.
