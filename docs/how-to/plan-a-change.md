# How to plan a change before implementing

This guide shows how to plan a change with the `plan` tool, which delegates
research and design to a read-only `PLAN` subagent before any workspace edits
happen. For the design rationale, see
[Plan](../explanation/agent-design/plan.md). For the planning tools, see
[Built-in tools](../reference/tools/index.md).

## Ask for a plan

Describe the task normally. When the work is complex, multi-file, or
architectural, the agent calls the `plan` tool itself; you can also ask for it
explicitly:

```text
Plan this change before implementing: rewrite the auth flow.
```

The `plan` tool spawns a `PLAN` subagent — read-only, plus a scoped write grant
to `.neenee/plans/` — that researches the codebase and writes the plan to a
file such as `.neenee/plans/rewrite-auth.md`. Its research runs in its own
context, so it does not bloat the main conversation.

## Approve the plan

When the subagent finishes, `plan` raises an *Approve* / *Keep planning*
prompt showing the plan path and a short excerpt. Choose:

- **Approve** to seed the todo list from the plan's `##` headings and start
  implementing.
- **Keep planning** to send feedback; the agent re-calls `plan` with the
  refined request.

There is no separate mode to enter or exit — the main agent always has its
full tool surface, and planning is just the call you approved.

## Track progress

On approval the agent seeds the unified **todo list** from the plan's `##`
headings (one item per section, starting `pending`). The list lives in the
**Activity** modal, showing the done/total ratio and a status glyph per step:

```text
Tasks  1/4
    ✓ Summary   ● Key Changes   ○ Test Plan   ○ Assumptions
```

The model marks steps `in_progress` → `completed` with the `todo`
(full-replace) or `todo_update` (mark one step) tools as it works. Steps it
has not touched yet show as `○ pending` — that is honest, not stale: it means
"not verified yet." To see the list at any time, open the **Activity** modal by
clicking the pinned activity bar. If a step stays on `○` after the model
claims it is done, ask it to update the list or run the verifier.

Calling `plan` again starts a fresh planning cycle and clears the list.

## Verify before declaring done

When the todo list is drained the workflow nudges the agent to call
`verify_plan_execution`, which runs the plan's `## Test Plan` commands and
grades each section `PASS` / `PARTIAL` / `FAIL`. A `PARTIAL` or `FAIL` should
send the agent back to address the gaps before it reports completion.

## See also

- [Plan](../explanation/agent-design/plan.md) — the subagent workflow and
  progression model
- [Built-in tools](../reference/tools/index.md) — `plan`, `todo`,
  `todo_update`, `verify_plan_execution`
