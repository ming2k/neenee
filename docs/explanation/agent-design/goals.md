# Goals

A **goal** is a durable, per-session objective the agent works toward across
many turns. It backs `/loop`, drives the progress checklist, and provides the
completion signal the autonomous loop trusts. This page is the mechanism deep
dive. For where goals fit in the control plane — retry, cancellation, the
uncapped autonomous loop — see [Harness architecture](harness.md).

## Why a dedicated goal primitive

Without a goal, an agent turn is stateless: the model decides when a task is
done by emitting a final message. Long, multi-turn work needs more than that:

1. **Durable intent.** An objective stated in turn 1 must still be active in
   turn 20 after a restart. The goal is persisted in SQLite keyed by session
   id, so it survives `/resume` and process restarts.
2. **Structured progress.** A flat "are we done yet" flag is too coarse. The
   goal carries a checklist the model updates, projected into the header, and
   **completion is deferred while checklist work remains**.
3. **A trusted termination signal.** `/loop` runs unsupervised iterations
   against the active goal; the loop needs a stable objective and a
   completion signal it can trust. See [Harness architecture](harness.md).

The goal carries **no status machine and no token or time budget**. An
earlier revision had a six-state lifecycle plus per-goal token accounting;
both were removed because the statuses were user-only and rarely useful, and
the budget flip behaved as a footgun more than a safeguard. See
[ADR-0010](../../../adr/0010-slim-goal-primitive.md) for that history, and
[ADR-0009](../../../adr/0009-uncapped-agentic-loop.md) for the uncapped loop
that made the iteration budget redundant.

## The slim primitive

A goal is three things:

| Field | Purpose |
|-------|---------|
| **Objective** | The WHAT — a durable statement of the end state |
| **Checklist** | A structured "definition of done" the model maintains |
| **`is_complete`** | A single boolean mirroring the persisted column |

The objective and `is_complete` persist to SQLite. The checklist is
**in-memory only** — it lives on the agent's runtime goal cell and is reset
to empty when the goal is reloaded from disk. The practical effect: the
checklist survives within a running session and across `/resume`, but is lost
on a crash or full process restart.

## Goal tools

Four tools are layered onto every agent so they share the live goal cell.
Parameter schemas live in [Built-in tools](../../reference/tools/index.md).

| Tool | Access | Purpose |
|------|--------|---------|
| `get_goal` | Read | Reads the current goal as JSON |
| `create_goal` | Write | Starts a new active goal (replaces a complete one) |
| `update_goal` | Write | Marks the goal complete |
| `goal_checklist` | Read | Replaces the in-memory checklist |

`create_goal` and `update_goal` are Write tools, so they pass through the
permission broker. `get_goal` and `goal_checklist` are Read tools and bypass
it. `update_goal` accepts only `complete`; an earlier `blocked` action was
removed with the status machine in [ADR-0010](../../../adr/0010-slim-goal-primitive.md).

## Checklist semantics

The checklist is structured progress the model maintains and the harness
trusts. Four hard rules are enforced before a `goal_checklist` write:

1. At most 50 items.
2. No item with empty content.
3. At most one `in_progress` item.
4. A non-empty checklist cannot be replaced with an empty list. Each item
   must receive a terminal `completed` or `cancelled` status.

Rule 4 is the subtle one: it forbids silently throwing away unfinished work.
The model must explicitly account for every item it introduced.

### Why the checklist gates completion

A goal can be marked complete only when the checklist is empty or every item
is `completed` or `cancelled`. The completion marker
`[NEENEE_GOAL_COMPLETE]` is the model's "objective done" signal, but the
harness re-checks the checklist before honoring it:

```text
model emits [NEENEE_GOAL_COMPLETE]
  └─ if every checklist item is completed or cancelled: mark complete in DB
  └─ else: strip the marker, emit "Goal completion was deferred because the
           checklist still has unfinished items."
```

The marker is always stripped from visible output — it is a control signal,
not prose. This is why an autonomous loop cannot shortcut its way out by
emitting the marker while checklist items remain pending or in progress.

## Two completion paths

Completion can arrive through either of two protocols, and both ultimately
call the same persistence routine:

| Path | Form | Where it is the primary exit |
|------|------|------------------------------|
| `[NEENEE_GOAL_COMPLETE]` marker | Plain text in the assistant message | The autonomous `/loop` |
| `update_goal(complete)` tool | A tool call through the permission broker | Interactive turns |

The interactive system prompt steers the model toward the `update_goal` tool,
because that path is visible to the user and passes the permission broker.
The `/loop` control prompt names the marker directly each iteration, because
an unsupervised loop needs a structured signal that natural turn termination
cannot provide — a no-tool-call assistant message happens every turn and
cannot distinguish "stopping for now" from "the goal is fully done." See
[ADR-0010](../../../adr/0010-slim-goal-primitive.md) for why the marker was
kept over relying on natural termination.

## Persistence

The store is one SQLite table keyed by `thread_id`, which is the active
session id. Because the key is the stable session id, simply resuming the
same session restores the same goal — there is no separate "goal resume"
step. The DB path is `data_dir/goals.db`. The legacy `token_budget`,
`tokens_used`, and `time_used_seconds` columns remain on the table for
backward compatibility with pre-0010 databases but are no longer read or
written.

On startup, if no thread-scoped goal exists, a one-time migration imports
any legacy goal from the old config keys. Those keys are no longer declared
on the live config, so the next save silently drops them: the migration is
one-way.

## Example lifecycle

```text
/goal ship the auth refactor
  └─ goal persisted in SQLite; agent.goal cell set; header refreshes

turn 1: model calls goal_checklist with 4 items (1 in_progress)
  └─ rule check passes → checklist updated → header renders "◎ ship the auth refactor [1/4]"

turn M: model emits [NEENEE_GOAL_COMPLETE]
  └─ checklist has 1 pending → defer
  └─ "Goal completion was deferred because the checklist still has unfinished items."

turn M+1: model calls goal_checklist, marks last item completed
turn M+2: model emits [NEENEE_GOAL_COMPLETE]
  └─ checklist fully resolved → mark complete → is_complete = true
  └─ loop, if running, stops on the completion signal
```

The `/goal status` report renders the same state textually, with per-item
labels.

## See also

- [Harness architecture](harness.md) — the control plane, the uncapped
  autonomous loop, and how completion interleaves with retry and cancellation
- [Built-in tools](../../reference/tools/index.md) — `get_goal`, `create_goal`,
  `update_goal`, `goal_checklist` parameter schemas
- [Slash commands](../../reference/commands.md) — `/goal` and `/loop`
- [Plan mode](plan-mode.md) — orthogonal to goal state; a goal and a loop can
  be active in either mode
- [ADR-0009](../../../adr/0009-uncapped-agentic-loop.md) — uncapped agentic loop
- [ADR-0010](../../../adr/0010-slim-goal-primitive.md) — slimming the goal
  primitive
