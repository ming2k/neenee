# Plan tools

`plan_enter`, `plan_exit`, `update_plan_progress`, and `verify_plan_execution`
drive the Plan-mode workflow. `PlanEnterTool`, `PlanExitTool`, and
`UpdatePlanProgressTool` are force-injected by `Agent::new` from
`crates/neenee-core/src/plan.rs` (any externally supplied copies are stripped
first). They share a `PlanToolContext` carrying the same
`Arc<Mutex<AgentMode>>` the `Agent` owns, so each tool flips the mode in place.
All three are `Read` and bypass the permission broker; after `plan_enter` or
`plan_exit` returns, the agent emits a `ModeChanged` event so the TUI refreshes
its mode indicator. The Plan-mode gate exempts `.neenee/plans/` writes through
`Tool::allowed_in_plan_mode`.

### `plan_enter`

No parameters. Switches the agent to `Plan` mode. The model calls it when a
request would benefit from designing before implementing; it should not be
called for simple tasks or when the user wants immediate implementation. See
[Plan mode](../../explanation/agent-design/plan-mode.md).

### `plan_exit`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `plan_path` | string | no | Path to the plan file under `.neenee/plans/` that was written |

Asks the user to approve the plan, then switches the agent back to `Build`
mode. The model calls it only after the plan is written and decision-complete.
On approval the mode flips, the `plan_path` is recorded as the active plan,
the plan body is read from disk and echoed back in the tool result, and the
markdown is parsed into `##` sections that drive the sticky progress panel
above the input box. If the user picks **Keep planning** the agent stays in
`Plan` mode. Manual `/mode build` skips the approval step. See
[Plan mode](../../explanation/agent-design/plan-mode.md).

### `update_plan_progress`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `section` | string | yes | Substring of the `##` heading to update (case-insensitive) |
| `status` | enum: `pending` / `in_progress` / `done` / `skipped` | yes | New status for the section |

Mark a section of the active plan. The agent calls this as it works through the
implementation so the sticky panel above the input box reflects the current
state. The section argument is matched case-insensitively as a substring of any
`##` heading, so the model does not have to echo the exact title. Has no effect
if there is no active plan (the call returns a clear "no active plan" hint
instead of erroring). See [Plan mode](../../explanation/agent-design/plan-mode.md).

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
[Plan verification](../../explanation/agent-design/subagents/plan-verification.md)
and [ADR-0012](../../adr/0012-toolaccess-tier-split.md).
