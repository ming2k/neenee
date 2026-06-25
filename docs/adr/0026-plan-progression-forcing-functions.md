# 0026. Plan progression forcing functions: plan-exit nudge, todo-continuation nudge, approval handoff

- **Status:** Superseded by ADR-0033
- **Date:** 2026-06-25

> Superseded by [ADR-0033](0033-remove-plan-and-verify-workflow.md). The
> plan-exit, todo-continuation, and verify nudges were removed.

## Context

Plan mode v2 (ADR-0006) added the approval gate, the active plan path, and the
plan-progress seed. The uncapped loop (ADR-0009) removed the per-turn round cap
so that *when the model drives*, nothing artificial stops it. Together they
left a gap that showed up every time a real plan was used: **the plan
workflow did not move on its own.**

Tracing each lifecycle transition to its driver made the gap precise. The
turn-loop exit had only two forcing functions (`agent.rs`):

- the **verify-nudge** gate — Build mode, active plan, `verify_plan_execution`
  not called this turn; one-shot; and it never read the todo list;
- the **pursue stop-gate** — opt-in via `/pursue` only (ADR-0015).

Round chaining carried the turn forward *only while the model kept calling
tools*. The moment the model emitted plain text, the turn ended unless one of
those two narrow gates fired. Three transitions therefore had no forcing
function at all:

1. **Planning → Approval.** A model that finished writing the plan but forgot
   to call `plan_exit` ended the turn on plain text. Nothing reminded it.
2. **Approval → Execution.** `execute_plan_exit` flipped the mode and returned
   a tool-result string ("Implement the plan now"), but injected no
   continuation. A weak model read that as a closing remark, replied "okay,
   starting now" as text, and the turn ended before any edit ran.
3. **Execution → Done.** The todo list (ADR-0020) was never consulted. A model
   could stop with five pending todos and the turn would end.

The verify-nudge made #3 worse, not better: it fired right after approval while
the todos were still all pending, asking for an audit of work that had not
started.

The reference point — Claude Code (reverse-engineered from its `2.1.190`
binary) — solves the same problem three ways: a **turn-end hard constraint**
(in Plan mode the turn must end with `ExitPlanMode` or `AskUserQuestion`); a
**post-approval synthetic instruction** baked into the `ExitPlanMode`
tool-result ("You can now start coding. Start with updating your todo list");
and a **todo-aware reminder loop** that re-injects when the list goes stale.

## Decision

Add three forcing functions to the turn-loop exit cascade, and re-order the
existing verify-nudge so each gate matches its phase. The cascade
(`agent.rs`, streaming and non-streaming loops, identical) is evaluated in
order; the first gate that fires injects a hidden user message and forces one
more round:

| # | Gate | State | Bound |
|---|------|-------|-------|
| 1 | **plan-exit nudge** | Plan mode, round ended, `plan_exit` not called this turn | one-shot per turn |
| 2 | **verify-nudge** | Build + active plan, todos drained, `verify_plan_execution` not called | one-shot per turn |
| 3 | **todo-continuation nudge** | Build + active plan, pending/in-progress todos remain | ≤ `MAX_TODO_NUDGES` (6) per turn |
| 4 | **pursue stop-gate** | `/pursue` armed, pursuit incomplete | ≤ `MAX_PURSUIT_ITERATIONS` (50) |

Concretely:

- **`TurnState`** gains `plan_exit_attempted_this_turn`, `plan_exit_nudged`,
  and `todo_nudges`. Dispatch sets `plan_exit_attempted_this_turn` when the
  model calls `plan_exit` (so a rejection that returns the model to Plan mode
  is not re-nudged immediately).
- **`should_nudge_plan_exit`** — Plan mode and the two flags say it has not
  tried and not been nudged.
- **`should_nudge_todos`** — Build mode, active plan, `has_pending_todos()`,
  and `todo_nudges < MAX_TODO_NUDGES`.
- **`should_nudge_verify`** gains `&& !self.has_pending_todos()`. Verification
  now belongs *after* the work: while todos remain, the todo-continuation
  nudge pushes progress; only once the list is drained does the verify-nudge
  ask for an independent audit.
- **`has_pending_todos`** reads the shared `TodoList` for any `Pending` or
  `InProgress` item — so the list (ADR-0020) becomes both the display and the
  forcing signal.
- **Approval handoff** (`PlanExitTool::call`, `plan.rs`) — the tool result now
  tells the model explicitly to start coding now, track progress with
  `todo` / `todo_update`, and not end the turn until the work is done or it is
  genuinely blocked. This is the Claude-Code-style synthetic instruction,
  delivered through the tool result rather than a separate injected message
  (see Alternatives).

`MAX_TODO_NUDGES = 6` (`lib.rs`, beside `MAX_PURSUIT_ITERATIONS`) bounds the
new forcing so a plan the model keeps refusing to advance cannot loop forever:
after six nudges the turn is allowed to end and the user resumes.

## Alternatives considered

- **A literal turn-end hard constraint, as Claude Code does.** Rejected: its
  model is "the turn *must* end with `ExitPlanMode`/`AskUserQuestion`,"
  enforced every round. neenee's loop is uncapped by ADR-0009 and its
  philosophy is opt-in forcing; a hard constraint would override the model's
  judgement (e.g. a legitimate "I need more info" text reply) and reintroduce
  a loop-shaped cap in disguise. A bounded nudge keeps the exit reachable
  while still pushing.

- **An unbounded todo gate (re-inject every exit until the list is empty).**
  Rejected: it is the verify-nudge's documented failure mode — "Without this
  the harness and model could ping-pong indefinitely." A model that emits
  text-only repeatedly would loop without bound. The cap (6) preserves
  ADR-0009's "the user can always interrupt / the turn always terminates"
  property while still moving a willing model on the first nudge.

- **Inject the post-approval instruction as a separate hidden user message**
  (a second message after the `plan_exit` tool result). Rejected: the tool
  result already returns to the model on the next round and is the natural
  channel for "what to do next." A separate injected message duplicates the
  signal and complicates the transcript. Strengthening the tool-result text
  (the Claude-Code approach) is one message, in the right place, with no
  dispatch change.

- **Make the todo-continuation gate fire for ad-hoc todos (no active plan).**
  Rejected: the gate is scoped to the plan workflow (`active_plan_path`
  required) deliberately. Ad-hoc todos without a plan are a scratchpad; a
  forcing gate over them would nag the model about lists it never agreed to
  drain. Plan-driven todos are the ones the user approved and expects
  completed.

- **Re-order the cascade so the todo-continuation nudge fires before
  verify-nudge unconditionally.** Rejected as redundant: gating the
  verify-nudge on `!has_pending_todos()` already makes the two mutually
  exclusive — while work remains the todo nudge fires; once it is done the
  verify nudge fires. Explicit ordering would express the same thing twice.

## Consequences

- **Positive:** the two transitions that previously stalled — forgetting
  `plan_exit`, and stopping before the todo list is empty — are now forced.
  The approval handoff no longer depends on the model remembering to start; a
  model that emits "okay, starting now" is caught by the todo-continuation
  nudge and re-injected with the pending list. The verify-nudge no longer
  fires prematurely (before any work), so verification happens in its right
  place. The change is local to the turn-loop exit and one tool-result string;
  no schema, persistence, or protocol change.

- **Negative:** a genuinely stuck plan can still end — by design, after six
  todo nudges. That is preferred over an unbounded loop, but it means the
  forcing is "push, then give up," not "drive to completion" (the latter is
  still `/pursue`'s job). The plan-exit nudge is one-shot, so a model that
  ignores it once is not re-nudged that turn.

- **Neutral:** ADR-0009's uncapped loop is intact — distinct tool calls remain
  uncapped; only the *forced re-injection* is bounded, exactly as the pursue
  stop-gate already is. The todo list (ADR-0020) gains a second consumer (the
  gate) reading the same status the TUI already shows. `/pursue` stays the
  only mechanism that drives a turn against the model's inclination without a
  plan; the new nudges are plan-scoped.

## References

- [ADR-0006](0006-plan-mode-v2.md) — the approval gate and active plan path
  these forcing functions sit behind.
- [ADR-0009](0009-uncapped-agentic-loop.md) — the uncapped loop; this ADR
  bounds only forced re-injection, not distinct tool calls.
- [ADR-0015](0015-pursue-stop-gate-and-repeat-cron.md) — the pursue stop-gate,
  the model for bounded within-turn forcing.
- [ADR-0020](0020-unified-task-list.md) — the unified todo list the
  todo-continuation gate reads.
- [ADR-0033](0033-remove-plan-and-verify-workflow.md) — the end-to-end
  workflow this ADR realised was later removed.
- Claude Code `2.1.190` binary (reverse-engineered) — turn-end constraint,
  ExitPlanMode tool-result instruction, and todo reminder loop; the prior art
  this ADR adapts, softened from a hard constraint to bounded nudges.
