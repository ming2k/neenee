# Plan tools

`plan_enter`, `plan_exit`, and `verify_plan_execution` drive the Plan-mode
workflow. `PlanEnterTool` and `PlanExitTool` are force-injected by
`Agent::new` from `crates/neenee-core/src/plan.rs` (any externally supplied
copies are stripped first). They share a `PlanToolContext` carrying the same
`Arc<Mutex<AgentMode>>` the `Agent` owns, so each tool flips the mode in place.
Both are `Read` and bypass the permission broker; after `plan_enter` or
`plan_exit` returns, the agent emits a `ModeChanged` event so the TUI refreshes
its mode indicator. The Plan-mode gate exempts `.neenee/plans/` writes through
`Tool::allowed_in_plan_mode`.

There is no separate plan-progress tool. When a plan is approved, `plan_exit`
seeds the [unified task list](interaction.md) (`todo` / `todo_update`) from the
plan's `##` headings, and the model tracks per-step progress with those tools.
See [ADR-0020](../../adr/0020-unified-task-list.md).

### `plan_enter`

No parameters. Switches the agent to `Plan` mode. The model calls it when a
request would benefit from designing before implementing; it should not be
called for simple tasks or when the user wants immediate implementation. It
also clears the active plan path and the task list, since re-entering Plan mode
starts a fresh planning cycle. See
[Plan mode](../../explanation/agent-design/plan-mode.md).

### `plan_exit`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `plan_path` | string | no | Path to the plan file under `.neenee/plans/` that was written |

Asks the user to approve the plan, then switches the agent back to `Build`
mode. The model calls it only after the plan is written and decision-complete.
On approval the mode flips, the `plan_path` is recorded as the active plan,
the plan body is read from disk and echoed back in the tool result, and the
markdown's `##` headings are seeded into the task list (one `Pending` item
each), which the model then tracks with `todo` / `todo_update`. If the user
picks **Keep planning** the agent stays in `Plan` mode. Manual `/mode build`
skips the approval step. See
[Plan mode](../../explanation/agent-design/plan-mode.md).

### `verify_plan_execution`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `focus` | string | no | Section name or concern to focus the verifier on |

Spawns an independent verifier sub-agent that re-reads the active plan and the
current workspace, then reports PASS / PARTIAL / FAIL per section with concrete
evidence. Call this before declaring the plan complete. Blocked in Plan mode
(no plan to verify). The verifier binds the `VERIFY` profile (ceiling
`Execute`), so it can run `bash` for tests/builds/type-checks as evidence but
cannot edit files, ask the user, or recurse. See
[Plan verification](../../explanation/agent-design/subagents.md#plan-verification)
and [ADR-0012](../../adr/0012-toolaccess-tier-split.md).
