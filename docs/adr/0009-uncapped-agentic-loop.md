# 0009. Uncapped agentic loop

- **Status:** Accepted
- **Date:** 2026-06-22

## Context

Before this decision, neenee wrapped its autonomous loop in **two** arbitrary
execution caps:

- **Per-turn round cap.** `MAX_TOOL_ROUNDS = 32` in
  `crates/neenee-agent/src/lib.rs` hard-paused a turn that called more than
  32 distinct tool rounds, with `SOFT_TOOL_ROUND_LIMIT = 26` injecting a
  hidden "convergence nudge" a few rounds earlier (`agent.rs`).
- **`/loop` iteration cap.** `1..=50` in `crates/neenee-cli/src/main.rs`
  bounded the outer autonomous loop. The model self-evaluated completion by
  emitting `[NEENEE_GOAL_COMPLETE]`, but if it never did, the loop ran out
  at 50 turns and reported `exhausted`.

Both caps pre-existed any survey of how peer agents handle the same problem.
A direct comparison of the two open implementations the user asked us to
study — codex (`codex-rs/core/src/session/turn.rs`) and claude-code
(`src/query.ts`) — produced a clear, converging answer:

| Concern | codex | claude-code |
|---|---|---|
| Agentic turn loop | `loop {}`, no per-iteration cap (`turn.rs:207`) | `while (true)`, no per-iteration cap (`query.ts:307`) |
| Iteration counter | none — there is no `max_turns`/`iteration_limit` | optional `maxTurns`, **unset on the main thread**; sub-agents cap at 200 (`forkSubagent.ts:65`) |
| Termination | model emits `end_turn: true` with no `tool_calls` | model emits an assistant message with no `tool_use` blocks |
| Runaway backstop | context-window pressure → auto-compaction, then `continue` (`turn.rs:304–323`) | same model, plus an explicit `taskBudget` token cap |
| `/loop` command | does not exist | exists, but is a **cron scheduler** (`/loop 5m <prompt>`), not an iteration driver |

Both treat the agentic loop as the **default execution model**: every
ordinary prompt already runs open-ended until the model itself stops calling
tools. Neither publishes a per-iteration counter as a user-facing knob.

neenee's two-layer cap system diverged from both. Worse, it produced
observable pain:

- The 32-round cap tripped on legitimate refactor turns that needed to read,
  edit, and verify several files in sequence. Users had to `/loop` to keep
  going, which started a fresh turn with a noisy control prompt.
- The 50-iteration cap turned into a budget the model would quietly exhaust
  on a hard sub-task, ending with `Loop exhausted its 50 iteration budget`.
  The user had no in-loop signal that the model was making progress vs.
  stuck — the cap was the signal, and it was the wrong one.
- Coupling between `/goal` and `/loop` (`/loop` refused to start without an
  active goal) created two-state management friction that codex and
  claude-code simply do not have.

The compaction backstop that *replaces* a hard cap was already in place
(`compaction_max_chars`, mid-turn pruning via `MidTurnCompactionGate`,
auto-compaction on context overflow). The cap was redundant safety theatre.

## Decision

1. **Remove the per-turn round cap.** Delete `MAX_TOOL_ROUNDS`,
   `SOFT_TOOL_ROUND_LIMIT`, and `CONVERGENCE_NUDGE`. The streaming and
   non-streaming turn loops (`Agent::run_streaming_with_events`,
   `Agent::run_with_events`) run until the model emits a final assistant
   message with no tool call, or until one of: user interrupt
   (`CancellationToken`), the repeated-call guard
   (`MAX_REPEATED_TOOL_CALLS = 3`), a provider/tool error, or context
   overflow after compaction. The `HarnessError::TurnLimitReached` variant
   and the `AgentResponse::TurnPaused` event are removed.

2. **Remove the `/loop` iteration cap.** `start_goal_loop` no longer takes
   a `max_iterations` parameter; it runs until the completion marker fires,
   the user runs `/loop stop` or `Esc`, a newer request supersedes it, or an
   error aborts. The `"exhausted"` terminal status is no longer produced.

3. **Decouple `/loop` from `/goal`.** `/loop <objective>` sets a fresh goal
   from the objective text and starts the loop in one step. `/loop` (no
   args) keeps the existing behaviour of starting on the active goal. The
   pure-numeric legacy form `/loop <N>` is rejected with a migration hint.

4. **Keep durable checkpoints.** `LoopCheckpoint.max_iterations` is preserved
   on the wire for backward compatibility, with `UNCAPPED_ITERATIONS =
   usize::MAX` as the new sentinel. `LoopCheckpoint::resume_iteration`
   continues to accept legacy finite-budget checkpoints (their cap is
   ignored on resume) so pre-ADR-0009 snapshots resume cleanly.

5. **Rely on compaction + user interrupt as the runaway backstop**, matching
   codex's explicit trust comment at `turn.rs:304`.

## Alternatives considered

- **Keep the caps, raise the numbers** (e.g. `MAX_TOOL_ROUNDS = 200`,
  `/loop 1..=500`). Rejected: any finite cap is arbitrary, produces the
  same "budget exhausted" failure mode at a slower cadence, and diverges
  from the two reference implementations. The cap is the wrong shape for
  the problem; compaction is the right shape.

- **Replace `/loop` with claude-code's cron semantics** (`/loop 5m <prompt>`
  schedules the prompt on a wall-clock interval). Rejected for this ADR:
  that is a different feature (long-running monitoring), not a replacement
  for the in-session autonomous loop. Can be added later under a different
  command name.

- **Drop `/loop` entirely and make every prompt an uncapped agentic loop**
  (the strict codex model). Rejected: neenee's goal/checklist accounting and
  durable resume are genuinely useful, and `/loop` is the natural entry
  point for them. Removing the cap and decoupling from `/goal` captures the
  relevant best practice without throwing away the durable infra.

- **Keep the convergence nudge, drop the hard cap.** Rejected: the nudge
  exists only to give the model a wind-down period before a hard wall; with
  no hard wall, the nudge is just an interruption. The model already
  receives "produce a final text answer when done" guidance in the system
  prompt.

## Consequences

Positive:

- Long refactor / multi-file verification turns no longer trip a 32-round
  cliff.
- `/loop` runs until the model genuinely completes or the user interrupts;
  no more "Loop exhausted its 50 iteration budget" on hard sub-tasks.
- `/loop <objective>` collapses two commands into one for the common case.
- Two fewer public-API variants to maintain (`TurnLimitReached`,
  `TurnPaused`) and one less notice severity (`NoticeSeverity::TurnLimit`),
  shrinking the harness-to-UI event surface.

Negative:

- A genuinely stuck model that makes *distinct* but unproductive tool calls
  can run longer before the user notices. Mitigation: the activity bar
  shows `turn N · round M · <status>` live, the user can interrupt at any
  time, and the repeated-call guard still catches the common case.
- `/loop`'s uncapped duration puts more weight on context compaction
  behaving well. The mitigation is the same as codex's: compaction has been
  in place and observed to hold the transcript well under the context limit.

Migration:

- The pure-numeric `/loop <N>` form is rejected with a message pointing to
  the new syntax. Non-numeric forms (`/loop resume`, `/loop status`,
  `/loop stop`, and now `/loop <objective>`) are unchanged or extended.
- Existing durable checkpoints with a finite `max_iterations` (legacy
  pre-ADR-0009 snapshots) still resume — the cap is ignored on resume.
- The `TurnLimitReached` error and `TurnPaused` event are gone; the TUI
  notice severity `TurnLimit` is gone. The transcript no longer renders
  amber "Turn paused after N tool rounds" notices.

## References

- `crates/neenee-agent/src/lib.rs` — `MAX_REPEATED_TOOL_CALLS` (the
  surviving guardrail), with comments documenting the uncapped-loop policy.
- `crates/neenee-agent/src/agent.rs` — the streaming and non-streaming turn
  loops, now without round caps.
- `crates/neenee-agent/src/orchestration.rs::start_goal_loop` — the uncapped
  outer loop.
- `crates/neenee-store/src/session.rs::UNCAPPED_ITERATIONS` — the wire
  sentinel for uncapped checkpoints.
- `crates/neenee-cli/src/main.rs` — the rewritten `/loop` parser.
- Peer implementations surveyed:
  `codex-rs/core/src/session/turn.rs:207,304–323,368` and
  `src/query.ts:307,829–835,1704–1712`.
- [Harness architecture](../explanation/agent-design/harness.md) — the
  surviving safety surface.
- [Turns and rounds](../explanation/agent-design/rounds-and-turns.md) —
  what ends a turn under the new model.
