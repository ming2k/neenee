# Pursuits and the pursue stop-gate

A **pursuit** is a durable, per-session objective. `/pursue <condition>` arms a
**stop-gate** that keeps the agent working toward that objective until the
model signals it is done — the autonomous "pursuit" is *within-turn*
continuation, not an outer loop of whole turns. This page is the mechanism
deep dive; for where it fits in the control plane see
[Harness architecture](harness.md), and for the clock-driven counterpart see
the [`/repeat` cron scheduler](#the-repeat-cron-scheduler) section below.

## Why a dedicated primitive

Without a pursuit, an agent turn is stateless: the model decides when a task is
done by emitting a final message. Long, multi-step work needs more than that:

1. **Durable intent.** An objective stated up front must still be active after
   a restart. The pursuit persists in SQLite keyed by session id, so it survives
   `/resume` and process restarts.
2. **A driver that does not give up early.** A single turn ends the moment the
   model stops calling tools, which often happens long before a real objective
   is achieved. The stop-gate refuses to let the turn end until the condition
   is met (or a safety cap is hit).
3. **A trusted termination signal.** The driver needs a structured "the
   objective is genuinely done" signal it can trust, distinct from a routine
   end-of-turn.

The pursuit carries **no status machine, no token/time budget, and no
checklist**. Earlier revisions had all three; they were removed because the
statuses were user-only, the budget flip was a footgun, and the checklist
added a second completion gate that rarely changed outcomes. See
[ADR-0010](../../../adr/0010-slim-pursuit-primitive.md) and
[ADR-0015](../../../adr/0015-pursue-stop-gate-and-repeat-cron.md) for that
history.

## The slim primitive

A pursuit is two things:

| Field | Purpose |
|-------|---------|
| **Objective** | The condition to pursue — a durable statement of the end state |
| **`is_complete`** | A single boolean mirroring the persisted column |

Both persist to SQLite (`data_dir/pursuits.db`).

## Pursuit tools

Three tools are layered onto every agent so they share the live pursuit cell.
Parameter schemas live in [Built-in tools](../../reference/tools/index.md).

| Tool | Access | Purpose |
|------|--------|---------|
| `get_pursuit` | Read | Reads the current pursuit as JSON |
| `start_pursuit` | Write | Starts a new active pursuit (replaces any existing one) |
| `complete_pursuit` | Write | Marks the pursuit complete |

`start_pursuit` and `complete_pursuit` are Write tools and pass through the
permission broker; `get_pursuit` is a Read tool and bypasses it. `complete_pursuit`
accepts only `complete`.

## The pursue stop-gate

`/pursue <condition>` does three things: persists the condition as the active
pursuit, **arms the stop-gate** on the agent, and drives one agent turn. The
gate sits at the turn-loop exit — the same place the verify-nudge gate already
forced one more model call instead of ending. On each exit it consults
`pursuit_continuation`, which returns a continuation prompt when **all** of
these hold:

- a pursuit is armed;
- an active (incomplete) pursuit exists;
- the latest response did **not** signal completion;
- the safety cap has not been reached.

When it returns a prompt, the gate injects the condition as a hidden
user-role message, bumps its iteration counter, and forces another round
instead of returning. The turn therefore runs to completion across many
rounds, re-injected each time the model tries to stop.

```text
/pursue make all tests pass and CI green
  └─ pursuit persisted; stop-gate armed; one turn begins

  round 1: model edits code, then tries to end the turn
    └─ gate: armed, pursuit incomplete, no completion signal → re-inject condition → round 2

  round N: model verifies, emits [NEENEE_PURSUIT_COMPLETE]
    └─ gate sees the completion signal → lets the turn end
    └─ orchestration finalizes: mark complete → is_complete = true
```

### Completion is a signal, not a judgement

There is no separate LLM "is the condition met?" judge on each stop. The
working model itself signals completion — by emitting the
`[NEENEE_PURSUIT_COMPLETE]` marker — and the gate trusts that signal (the gate
*gates*, the model *signals*). This matches Claude Code's stop-hook `/pursuit`,
avoids a model call per stop, and keeps the decision deterministic.

The two completion paths both call the same persistence routine:

| Path | Form | Typical use |
|------|------|-------------|
| `[NEENEE_PURSUIT_COMPLETE]` marker | Plain text in the assistant message | A running pursuit |
| `complete_pursuit(complete)` tool | A tool call through the permission broker | Interactive turns |

The marker is always stripped from visible output — it is a control signal,
not prose.

### Safety cap

`MAX_PURSUIT_ITERATIONS` (50 rounds) bounds a pursuit that never signals
completion. Hitting it disarms the gate and ends the turn with a notice, so a
stuck pursuit cannot loop forever. The user can also interrupt at any time
with `Esc` or `/pursue stop`.

## The `/repeat` cron scheduler

Orthogonal to pursuits, `/repeat <cron> <prompt>` schedules a prompt on a
**clock**. It is a fully separate subsystem — the two driving dimensions are
deliberately distinct:

| | `/pursue` | `/repeat` |
|---|---|---|
| Driver | a condition (stop-gate) | a clock (cron) |
| Work unit | continuation within one turn | a fresh turn per tick |
| Stops when | the condition is met / cap / interrupt | cancelled or auto-expired |
| Persistence | pursuit in `pursuits.db` | jobs in `repeat.db` |

`/repeat` parses a five-field cron expression (`minute hour day month weekday`,
e.g. `*/5 * * * *` for every five minutes, `0 9 * * 1-5` for 09:00 on
weekdays), stores the job durably, runs the first fire immediately, and a
background scheduler ticks every 30 s to fire due jobs as normal chat turns.
Jobs auto-expire after 30 days. See [Slash commands](../../reference/commands.md)
for the command surface.

## Persistence

The pursuit store is one SQLite table keyed by `thread_id` (the active session
id), so resuming the same session restores the same pursuit — there is no
separate "pursuit resume" step. The legacy `token_budget`, `tokens_used`, and
`time_used_seconds` columns remain for backward compatibility with pre-0010
databases but are no longer read or written.

On startup, if no thread-scoped pursuit exists, a one-time migration imports any
legacy pursuit from the old config keys. That migration is one-way.

A `PursuitCheckpoint` is written while a pursuit runs (status
running/completed/interrupted/error) so `/session status` can report it, but a
pursuit is a single turn — the checkpoint is observability, not a resumable
multi-turn loop.

## See also

- [Harness architecture](harness.md) — the control plane, the stop-gate's
  place beside the verify-nudge gate, and how completion interleaves with
  retry and cancellation
- [Built-in tools](../../reference/tools/index.md) — `get_pursuit`, `start_pursuit`,
  `complete_pursuit` parameter schemas
- [Slash commands](../../reference/commands.md) — `/pursue` and `/repeat`
- [ADR-0015](../../../adr/0015-pursue-stop-gate-and-repeat-cron.md) — the
  decision to replace `/pursuit` + `/loop` with the stop-gate + cron scheduler
- [ADR-0010](../../../adr/0010-slim-pursuit-primitive.md) — slimming the pursuit
  primitive
