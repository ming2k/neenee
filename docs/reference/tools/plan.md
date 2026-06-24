# Plan tools

`plan` and `verify_plan_execution` drive the plan workflow. `plan` is injected
by `Agent::new` from `crates/neenee-agent/src/plan_subagent.rs`; it wraps the
dispatch tool bound to the `PLAN` profile and gates the result behind user
approval. It is `Read` and bypasses the permission broker; the spawned `PLAN`
subagent carries its own `WriteScope` grant scoped to `.neenee/plans/`
([ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md)). There is no
separate plan-progress tool: when a plan is approved, `plan` seeds the
[unified todo list](interaction.md) (`todo` / `todo_update`) from the plan's
`##` headings. See [ADR-0020](../../adr/0020-unified-task-list.md) and
[ADR-0027](../../adr/0027-plan-as-subagent.md).

### `plan`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `request` | string | yes | The change to plan, with enough context for the subagent to research and design it self-contained |

Spawns a read-only `PLAN` subagent that researches the codebase and writes the
plan to `.neenee/plans/<slug>.md` (its `## ` headings become the todo list on
approval). When the subagent returns, `plan` raises an *Approve* / *Keep
planning* prompt:

- **Approve** — records the plan path as the active plan, seeds the todo list
  from the plan's `##` headings, and returns a handoff instruction telling the
  model to start coding and track progress with `todo` / `todo_update`.
- **Keep planning** — returns the user's feedback to the model, which re-calls
  `plan` with the refined request.

The model calls `plan` when a request would benefit from designing before
implementing; it should not be called for simple tasks or when the user wants
immediate implementation. Because `plan` spawns a subagent, every subagent
profile excludes it — planning cannot recurse. See
[Plan](../../explanation/agent-design/plan.md).

### `verify_plan_execution`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `focus` | string | no | Section name or concern to focus the verifier on |

Runs a two-phase pipeline against the active plan: deterministic checks (it
parses and runs the commands in the plan's `## Test Plan` section) followed by
a lightweight model review that grades each `##` section `PASS` / `PARTIAL` /
`FAIL` and ends with a one-line `VERDICT:`. Call this before declaring the
plan complete. Requires an active plan (set by an approved `plan`). See
[Plan](../../explanation/agent-design/plan.md#verifying-the-plan) and
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).
