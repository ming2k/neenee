# 0010. Slim the goal primitive

- **Status:** Accepted
- **Date:** 2026-06-22

## Context

ADR-0009 removed the per-turn round cap and the `/loop` iteration cap. That
left the **goal primitive** itself carrying a status machine, a token budget,
elapsed-time accounting, four non-default non-terminal statuses, and a
self-evaluation marker — most of which find no equivalent in either of the
two reference implementations surveyed in ADR-0009:

| neenee (pre-0010) | codex | claude-code |
|---|---|---|
| `GoalStatus { Active, Paused, Blocked, UsageLimited, BudgetLimited, Complete }` | no goal primitive at all — the agentic loop is the unit of work | no goal primitive — the agentic loop is the unit; `taskBudget` is a *token* cap on the loop, not a per-goal attribute |
| `token_budget` per goal + SQL accounting + `BudgetLimited` status | nothing | `taskBudget` on `query()` is optional and rarely used; main thread leaves it unset |
| `[NEENEE_GOAL_COMPLETE]` marker self-evaluation | model emits `end_turn: true` with no tool calls | model emits an assistant message with no `tool_use` blocks |
| `Paused` / `Blocked` / `UsageLimited` statuses | nothing | nothing — the user just stops sending input |
| `pause` / `resume` / `mark_blocked` service methods + slash commands | nothing | nothing |
| `time_used_seconds` accounting | nothing | nothing |

Concrete waste observed in the survey:

- **`UsageLimited` is dead.** Documented in `goals.md:65-67` as never
  produced by any code path.
- **`Paused` / `Blocked` are user-only and rarely useful.** The model can
  set `Blocked` via `update_goal`, but `pause`/`resume`/`budget` exist only
  as `/goal` slash commands. None of them survive a session restart as a
  useful resumption point — the user typically just runs another turn.
- **Token budget is unused in practice.** No user-facing telemetry surfaces
  it as a setting the user actually tunes; the `BudgetLimited` flip is
  mostly a footgun.
- **`should_auto_continue` is dead code** (defined but never called).
- **`GoalService::Clone` recreates its `accounting_lock` semaphore**
  (`service.rs:13-20`), so the lock provides no actual mutual exclusion
  across clones — every context (`TurnContext`, `InteractiveTurnContext`,
  `LoopRunContext`) holds its own independent permit pool. The lock was
  theatre.
- **Two parallel render paths.** The TUI's `draw_goal_bar` already reads
  only `status` / `objective` / `checklist` — never the budget fields.
  Budget fields surface only in the textual `/goal status` render. Slimming
  them off the primitive does not break the TUI loop.

Meanwhile, the genuinely useful parts of the goal primitive —
**durable intent across restarts**, **multi-step checklist gating**, and
**the structured `[NEENEE_GOAL_COMPLETE]` signal that lets `/loop`
terminate** — have no peer in codex / claude-code and are worth keeping.

## Decision

1. **Collapse the status machine.** Replace `GoalStatus { Active, Paused,
   Blocked, UsageLimited, BudgetLimited, Complete }` with a single
   `is_complete: bool` on both `Goal` (runtime) and `ThreadGoal`
   (persisted). The terminal/transition table, `pause`, `resume`,
   `mark_blocked`, and `should_auto_continue` are removed.

2. **Drop token / time accounting.** Remove `token_budget`, `tokens_used`,
   `time_used_seconds` from `Goal` and `ThreadGoal`. Remove
   `Goal::remaining_tokens`, `GoalService::account_turn`,
   `GoalAccountingResult`, the `accounting_lock` semaphore, and
   `GoalStore::account_usage`. `TurnOutcome.token_usage` survives (it is
   per-turn telemetry, not goal accounting).

3. **Slim `/goal` subcommands.** Keep `/goal`, `/goal status`,
   `/goal <objective>`, `/goal edit`, `/goal done`, `/goal clear`. Remove
   `/goal pause`, `/goal resume`, `/goal budget <tokens>`,
   `/goal budget clear`. Update `/help` text.

4. **Slim the model-facing tools.** `CreateGoalTool` loses its
   `token_budget` parameter. `UpdateGoalTool` loses the `blocked` action —
   only `complete` remains. Their descriptions are rewritten to match.

5. **Keep the durable surface that pulls its weight:**
   - `objective` — the WHAT.
   - `checklist` (`GoalChecklistItem`, `GoalChecklistStatus`) — multi-step
     gating of completion via `Goal::can_complete()`.
   - `[NEENEE_GOAL_COMPLETE]` marker — the structured signal that lets
     `/loop` terminate cleanly. The harness still strips it from visible
     output and still defers completion while checklist items remain.
   - SQLite persistence — `objective`, `is_complete`, timestamps.
   - `/loop resume` continues to work via `LoopCheckpoint`.

6. **SQLite compatibility.** The columns `token_budget`, `tokens_used`,
   `time_used_seconds` are kept on the `thread_goals` table for legacy
   databases but no longer read or written. The `status` column keeps its
   TEXT shape but only two values are written going forward: `"active"`
   and `"complete"`. On read, any pre-0010 status (`paused`, `blocked`,
   `usage_limited`, `budget_limited`) is mapped to `active` — the user
   loses paused/blocked state across the upgrade, which is acceptable
   because those states are being removed anyway.

## Alternatives considered

- **Drop the goal primitive entirely (the strict codex / claude-code
  model).** Rejected, as in ADR-0009: the durable objective + checklist +
  marker combination has no replacement in those codebases, and `/loop`
  needs *some* termination signal. Removing the goal would force a
  heuristic "did the model say it's done?" check, which is worse than the
  marker.

- **Keep the marker, drop the checklist too.** Rejected: the checklist is
  the one piece that actively prevents premature `done` declarations. It
  is the structured "definition of done" that codex and claude-code lack
  but neenee benefits from.

- **Replace `is_complete: bool` with a two-variant enum
  `GoalStatus { Active, Complete }`.** Rejected: same information, more
  syntax. The bool composes cleanly with the marker + checklist gate and
  reads better at call sites (`if goal.is_complete` vs
  `if goal.status == GoalStatus::Complete`).

- **Drop the `[NEENEE_GOAL_COMPLETE]` marker and rely on natural turn
  termination.** Rejected: `/loop` needs a structured signal. Natural
  termination (no tool calls) happens every turn — it cannot distinguish
  "I'm stopping for now" from "the goal is fully done." The marker is the
  minimal signal that resolves that ambiguity.

- **Schema migration that drops the unused columns.** Rejected for this
  ADR: `CREATE TABLE IF NOT EXISTS` is the entire migration system
  (`store.rs:79-89`), and a `ALTER TABLE … DROP COLUMN` would need a
  versioned migration framework the project does not have. Ignoring the
  columns is cheap (four bytes per row, never read); a future ADR can
  introduce migration versioning and clean them up.

## Consequences

Positive:

- The goal primitive fits in one screen: `objective`, `checklist`,
  `is_complete`. The state machine, budget, accounting lock, and elapsed
  time are gone.
- `/goal` subcommands shrink from 9 forms to 6, all of which do something
  a user can observe.
- `GoalService` loses six methods and a semaphore; the SQL layer loses
  `account_usage` and four of the five `update_goal` shapes.
- No more dead `UsageLimited` variant, no more lock-as-theatre.
- The TUI renders the same goal bar it always did — the budget bar and
  elapsed-time line in `/goal status` are gone, which most users will not
  notice.

Negative:

- Users who actually used `Paused` / `Blocked` / token budgets lose them.
  Mitigation: the activities those states represented (pause work, hit a
  budget wall) are better expressed as `/loop stop` and `/clear`, which
  already exist.
- Pre-0010 paused/blocked/budget-limited goals re-activate on first load
  after the upgrade. Users will see a previously-paused goal become
  active in the bar; they can `/goal clear` it. This is a one-time
  migration cost.
- Token-budget users (if any) lose the auto-stop behaviour. The uncapped
  loop (ADR-0009) plus `Esc` / `/loop stop` is the replacement.

## References

- `crates/neenee-core/src/goals/mod.rs` — slimmed `Goal`, `ThreadGoal`,
  `GoalChecklistStatus`, `GoalChecklistItem`.
- `crates/neenee-core/src/goals/service.rs` — slimmed `GoalService`.
- `crates/neenee-core/src/goals/store.rs` — column-compatible schema,
  legacy-status mapping.
- `crates/neenee-core/src/goals/tools.rs` — simplified `CreateGoalTool`
  and `UpdateGoalTool`.
- `crates/neenee-agent/src/orchestration.rs::execute_turn` — no more
  `account_turn` call or budget notices.
- `crates/neenee-cli/src/main.rs` — slimmed `/goal` parser and
  `format_goal_status`.
- `crates/neenee-cli/src/tui/render/chrome.rs::draw_goal_bar` —
  `goal.is_complete` guard replaces the status enum check.
- [Pursuits](../explanation/agent-design/pursuits.md) — rewritten around the
  slimmed primitive.
- [Harness architecture](../explanation/agent-design/harness.md) —
  goal-state and autonomous-loop sections updated.
- Predecessor: [ADR-0009](0009-uncapped-agentic-loop.md) — uncapped
  agentic loop, which set up this slimming.
