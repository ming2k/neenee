# Goals

A **goal** is a durable, per-session objective the agent works toward across
many turns. It is the unit that backs `/loop`, drives the progress checklist in
the header, and enforces a token budget. This page is the mechanism deep dive.
For where goals fit in the control plane — retry, cancellation, the uncapped
autonomous loop — see [Harness architecture](harness.md).

## Why a dedicated goal primitive

Without a goal, an agent turn is stateless: the model decides when a task is
done by emitting a final message. Long, multi-turn work needs more than that:

1. **Durable intent.** An objective stated in turn 1 must still be active in
   turn 20 after a restart. The goal is persisted in SQLite keyed by session
   id, so it survives `/resume` and process restarts.
2. **Structured progress.** A flat "are we done yet" flag is too coarse. The
   goal carries a checklist the model updates, projected into the header as
   `done/total`, and **completion is deferred while checklist work remains**.
3. **Bounded autonomy.** `/loop` runs unsupervised iterations against the
   active goal; the loop needs a stable objective and a completion signal it
   can trust. See [Harness architecture](harness.md).
4. **Cost ceiling.** A token budget turns cumulative spend into a hard limit
   the harness enforces, not a guideline the model polices itself.

## Two views of a goal

The goal has two shapes, kept deliberately apart (`crates/neenee-core/src/goals/mod.rs`):

| Type | Lives in | Carries checklist? | Purpose |
|------|----------|--------------------|---------|
| `ThreadGoal` (`mod.rs:13`) | SQLite `thread_goals` table | No | Persisted durability: status, budget, usage, timestamps |
| `Goal` (`mod.rs:27`) | In-memory `Arc<Mutex<Option<Goal>>>` on the `Agent` | Yes | Runtime view exposed to tools and the TUI |

The checklist is **not persisted**. It lives only on the agent's in-memory
`goal` cell (`crates/neenee-agent/src/agent.rs:63`). When `GoalService` loads a
`ThreadGoal` from disk it resets `checklist` to an empty `Vec`
(`crates/neenee-core/src/goals/service.rs:251`); `refresh_agent_goal`
preserves the live checklist across such a refresh
(`crates/neenee-agent/src/orchestration.rs:209`). The practical effect: the
checklist survives `/resume` within a session because the same agent process
rehydrates it, and survives status transitions, but is lost on a crash.
Persisting it is a deferred follow-up.

## Status machine

`GoalStatus` (`mod.rs:41`) has six variants. The authoritative transition
table is `GoalService::update_goal_status`
(`crates/neenee-core/src/goals/service.rs:155`):

| From \ To | `Active` | `Paused` | `Blocked` | `UsageLimited` | `BudgetLimited` | `Complete` |
|-----------|----------|----------|-----------|----------------|-----------------|------------|
| `Active` | — | user `/goal pause` | model `update_goal` | — | harness (SQL) | user `/goal done`, model, marker |
| `Paused` | `/goal resume` | — | — | — | — | — |
| `Blocked` | `/goal resume` | — | — | — | — | `/goal done` |
| `UsageLimited` | `/goal resume` | — | — | — | — | `/goal done` |
| `BudgetLimited` | `/goal resume` (if budget raised) | — | — | — | — | `/goal done` |
| `Complete` | `/goal`, `create_goal`, `/loop` | — | — | — | — | — |

Two notes that do not fit a cell:

- `is_terminal` returns true only for `BudgetLimited` and `Complete`
  (`mod.rs:68`); `can_be_resumed` covers the four intermediate states
  (`mod.rs:72`).
- `UsageLimited` is **defined, serialized, and accepted by the table but never
  produced by any code path today**. It is reserved for a future
  provider-usage-cap integration.

`BudgetLimited` is the one status the Rust table does not produce. It is set
inside the accounting SQL (see [Budget enforcement](#budget-enforcement)) and
by a `CASE` clause that re-clamps any `Active` that would breach the budget.

A second validation layer runs in SQL inside `GoalStore::update_goal`
(`crates/neenee-core/src/goals/store.rs:230`): a `CASE` expression keeps
`status = 'budget_limited'` if the requested status was `paused` or `blocked`,
and another flips a requested `active` to `budget_limited` whenever
`tokens_used >= token_budget`. The Rust service validates intent; the store
guarantees the budget invariant.

## Goal tools

Four tools are force-injected by `Agent::new`
(`crates/neenee-agent/src/agent.rs:117`) so they share the agent's live
`thread_id` and `goal` cells. Parameter schemas live in
[Built-in tools](../../reference/tools/index.md).

| Tool | Access | Triggers |
|------|--------|----------|
| `get_goal` | `Read` | Reads the current goal as JSON |
| `create_goal` | `Write` | Starts a new active goal (replaces a complete one) |
| `update_goal` | `Write` | Marks the goal `complete` or `blocked` |
| `goal_checklist` | `Read` | Replaces the in-memory checklist |

`create_goal` and `update_goal` are `Write` so they pass through the
permission broker; both override `permission_label` and
`permission_description` so the prompt reads `Create goal` / `Update goal`
with a plain-language body, not the model-facing instruction prose
(`crates/neenee-core/src/goals/tools.rs:113`). `get_goal` and
`goal_checklist` are `Read` and bypass the broker.

`update_goal` accepts only `complete` or `blocked`. The tool description
encodes the blocking policy: `blocked` is reserved for a blocking condition
that has **recurred for at least three consecutive goal turns**, not for work
that is merely hard or slow.

## Checklist semantics

The checklist is structured progress the model maintains and the harness
trusts. `goal_checklist` writes directly into the agent's live `goal` cell
(`crates/neenee-core/src/goals/tools.rs:299`), then the agent emits
`AgentEvent::GoalUpdated` so the TUI refreshes (`crates/neenee-agent/src/agent.rs:1246`).

Four hard rules are enforced before the write
(`crates/neenee-core/src/goals/tools.rs:271`):

1. At most 50 items.
2. No item with empty content.
3. At most one `in_progress` item.
4. A non-empty checklist cannot be replaced with an empty list. Each item must
   receive a terminal `completed` or `cancelled` status.

Rule 4 is the subtle one: it forbids silently throwing away unfinished work.
The model must explicitly account for every item it introduced.

### Why the checklist gates completion

`Goal::can_complete` (`mod.rs:112`) is true only when the checklist is empty or
every item is `completed` or `cancelled`. The completion marker
`[NEENEE_GOAL_COMPLETE]` (`crates/neenee-core/src/lib.rs:9`) is the model's
"objective done" signal, but the harness re-checks `can_complete` before
honoring it (`crates/neenee-agent/src/orchestration.rs:498`):

```text
model emits [NEENEE_GOAL_COMPLETE]
  └─ if goal.can_complete(): mark_complete in DB
  └─ else: strip marker, emit "Goal completion was deferred because the
           checklist still has unfinished items."
```

The marker is always stripped from visible output (`orchestration.rs:519` and
`orchestration.rs:602`) — it is a control signal, not prose. This is why an
autonomous loop cannot shortcut its way out by emitting the marker while
checklist items remain pending or in progress.

## Budget enforcement

Each turn's token and elapsed-time cost is accounted against the active goal
inside `execute_turn` (`crates/neenee-agent/src/orchestration.rs:478`). The
`GoalStore::account_usage` SQL does the accounting and the status flip in one
statement (`crates/neenee-core/src/goals/store.rs:396`):

```sql
UPDATE thread_goals
SET time_used_seconds = time_used_seconds + ?1,
    tokens_used       = tokens_used + ?2,
    status = CASE
        WHEN status = 'active' AND token_budget IS NOT NULL
             AND (tokens_used + ?2) >= token_budget
        THEN 'budget_limited' ELSE status END,
    updated_at_ms = ?3
WHERE thread_id = ?4
  AND (?5 IS NULL OR goal_id = ?5)
  AND status IN ('active', 'budget_limited')
```

The `goal_id` guard means a turn that finished after the user replaced the
goal is silently dropped — stale accounting cannot land on a new goal. When
the flip fires, the harness emits a user-visible message and the next system
prompt reflects `BudgetLimited` so the model knows to wrap up. `/goal budget
<tokens>` raises the ceiling; `/goal resume` re-promotes to `Active` only if
the new budget covers the spend, otherwise the goal stays `BudgetLimited`
(`crates/neenee-core/src/goals/service.rs:121`).

## Persistence and legacy migration

The store is one SQLite table keyed by `thread_id`
(`crates/neenee-core/src/goals/store.rs:10`). The DB path is
`data_dir/goals.db` via `paths::get().goals_db()`; `thread_id` is the active
session id. Because the key is the stable session id, simply resuming the same
session restores the same goal — there is no separate "goal resume" step.

On startup, if no thread-scoped goal exists, a one-time migration imports any
legacy goal from the old config keys (`harness_goal`,
`harness_goal_completed`, `harness_goal_checklist`) at
`crates/neenee-cli/src/main.rs:49`. Those keys are no longer declared on the
live `Config` struct, so the next save silently drops them: the migration is
one-way.

## Example lifecycle

```text
/goal ship the auth refactor
  └─ goal_service.set_goal → ThreadGoal{status: Active} in SQLite
  └─ agent.goal = Some(Goal{checklist: []}) ; emit GoalUpdated

turn 1: model calls goal_checklist with 4 items (1 in_progress)
  └─ rule check passes → goal.checklist updated → emit GoalUpdated
  └─ header renders "◎ ship the auth refactor [1/4]"

turn N: each completed turn's TokenUsage accounted in account_usage SQL
  └─ if tokens_used + delta >= budget → status flips to BudgetLimited

turn M: model emits [NEENEE_GOAL_COMPLETE]
  └─ can_complete()? checklist has 1 pending → defer
  └─ "Goal completion was deferred because the checklist still has unfinished items."

turn M+1: model calls goal_checklist, marks last item completed
turn M+2: model emits [NEENEE_GOAL_COMPLETE]
  └─ can_complete()? yes → mark_complete → status Complete
  └─ loop, if running, stops early on the boolean return from execute_turn
```

The `/goal status` report (`crates/neenee-cli/src/main.rs:1898`) renders the
same state textually, including a budget bar and per-item labels.

## See also

- [Harness architecture](harness.md) — the control plane, the uncapped
  autonomous loop, and how goal accounting interleaves with retry and
  cancellation
- [Built-in tools](../../reference/tools/index.md) — `get_goal`, `create_goal`,
  `update_goal`, `goal_checklist` parameter schemas
- [Slash commands](../../reference/commands.md) — `/goal` and `/loop`
- [Plan mode](plan-mode.md) — orthogonal to goal state; a goal and a loop can
  be active in either mode
