# Plan mode

Plan mode is a read-only execution surface for researching and designing a
change before any workspace edits happen. It exists so a complex request can be
investigated and written up as a plan without the model mutating source files
halfway through understanding them.

Plan mode is one of two **agent modes**. The mode is a property of the
`Agent` control plane, not of a provider or a session, and it gates tool
execution on every ReAct round.

## The two modes

`AgentMode` (`crates/neenee-core/src/lib.rs`) has two variants:

| Mode | Tool surface | Entered by |
|------|--------------|------------|
| `Build` | Every tool, subject to the permission broker | Default; `/mode build`; `plan_exit` |
| `Plan` | Read-only tools plus writes under `.neenee/plans/` only | `/mode plan`; `plan_enter` |

The active mode is shown in the TUI header accent color and can be switched
manually with the [`/mode`](../../reference/commands.md) command at any time.

## Entering and exiting

There are two independent ways to switch mode. Either one updates the same
shared mode cell, so they never disagree.

### Manual: `/mode`

The user runs `/mode plan` or `/mode build` directly. This is the authoritative
override: it works regardless of what the model is doing and is how a user
forces the agent back into `Build` mode if planning has gone far enough.

### Autonomous: `plan_enter` and `plan_exit`

The model can switch modes itself through two built-in tools that
`Agent::new` injects alongside the goal tools (`crates/neenee-core/src/plan.rs`):

| Tool | Access | Effect |
|------|--------|--------|
| `plan_enter` | `Read` | Switch to `Plan` mode |
| `plan_exit` | `Read` | Switch back to `Build` mode; optional `plan_path` records the plan file |

The system prompt describes the workflow: when a request is complex, spans
multiple files, or would benefit from designing first, the model calls
`plan_enter`; it researches with read-only tools, writes the plan to
`.neenee/plans/<name>.md`, then calls `plan_exit` to return to `Build` mode and
implement the plan.

The switch is automatic and unconditional; neenee does not prompt the user to
confirm an autonomous mode change. The user stays in control through `/mode`
and can revert at any time. See [Built-in tools](../../reference/tools.md) for the
parameter schemas.

## The plan-file write exemption

Plan mode is read-only, with one deliberate exception. `write_file` and
`edit_file` may write files under `.neenee/plans/` while planning, so the model
can persist the plan document it is supposed to produce. Every other write
target remains blocked.

The exemption is implemented per-invocation, not per-tool:

1. The `Tool` trait exposes `allowed_in_plan_mode(arguments)`
   (`crates/neenee-core/src/lib.rs`). Its default is `access() == Read`.
2. `WriteFileTool` and `EditFileTool` override it to return `true` only when
   the resolved `path` sits inside `.neenee/plans/`, using
   `plan::is_plan_path` (`crates/neenee-core/src/plan.rs`).
3. The Plan-mode gate in `Agent::execute_tool` consults that method instead of
   `ToolAccess` alone:

   ```text
   if mode == Plan && !tool.allowed_in_plan_mode(arguments) {
       return "[Plan mode] Tool '<name>' is blocked. ..."
   }
   ```

Path resolution canonicalizes the parent directory and re-appends the file name,
so a brand-new plan file that does not exist yet still resolves correctly. The
exemption only relaxes the Plan-mode gate; the permission broker still applies,
so a plan-file write still follows the normal once/always/reject flow.

## How a mode switch propagates

A mode change takes effect immediately and is visible everywhere that reads
mode state:

- The mode cell is an `Arc<Mutex<AgentMode>>` shared between the `Agent` and
  the plan tools through `PlanToolContext`. `plan_enter` and `plan_exit` mutate
  it in place.
- The system prompt is rebuilt before every model round by
  `Agent::ensure_system_prompt`, so the round after a switch sees the correct
  mode description and tool restrictions.
- After `plan_enter` or `plan_exit` returns, the agent emits a
  `ModeChanged` event, which is relayed to the TUI so the header indicator
  refreshes live, mid-turn.

Because the gate is re-evaluated on every tool call, a single turn can cross
the boundary: the model calls `plan_enter`, researches in `Plan` mode, writes
the plan, then calls `plan_exit` and continues implementing in `Build` mode,
all within one agent run.

## Relationship to goals and the autonomous loop

Plan mode is orthogonal to [goal state](harness.md) and the
[autonomous loop](harness.md). A goal and a loop can be active in either mode.
In practice the model usually enters `Plan` mode first, exits to `Build` once
the plan is written, and then pursues the goal with full tool access. The mode
does not affect goal accounting, the completion marker, or loop iteration
budgets.

## Plan progress panel

Once `plan_exit` is approved the agent parses the plan markdown into sections
(one per `##` heading) and shows them in a sticky 3-row panel above the input
box:

```text
╭── Plan: rewrite-auth.md · 1/4 done ───────────────╮
│ ✓ Summary  ● Key Changes  ○ Test Plan  ○ Assump… │
╰───────────────────────────────────────────────────╯
```

The panel is hidden when no plan is active, when the view is zoomed into a
sub-agent (the plan belongs to the parent context), and while an overlay
modal is open. It re-appears as soon as those conditions clear.

Section status is **model-driven, not inferred**. The system prompt
instructs the model to call `update_plan_progress(section, status)`
whenever it starts or finishes a section. A section the model forgets to
mark stays `Pending` — which is honest (the work has not been verified)
rather than a stale auto-progress that misleads.

Status glyphs:

| Glyph | Status      | Color           | Meaning                                |
|-------|-------------|-----------------|----------------------------------------|
| `✓`   | Done        | `theme.ok()`    | Section complete and verified          |
| `●`   | InProgress  | `theme.warn()`  | Currently being worked on              |
| `○`   | Pending     | `theme.muted()` | Not started yet                        |
| `—`   | Skipped     | `theme.muted()` | Turned out not to apply                |

The progress snapshot is persisted in `session.json` via
`SessionEvent::PlanProgressSet`, so resume restores the panel in the same
state. See [ADR-0007](../../adr/0007-plan-progress-panel.md) for the design
rationale.

## See also

- [Built-in tools](../../reference/tools.md) — `plan_enter`, `plan_exit`, and
  `update_plan_progress` parameter schemas, and the
  `allowed_in_plan_mode` access rule
- [Slash commands](../../reference/commands.md) — the `/mode` command
- [Harness architecture](harness.md) — the control plane that Plan mode
  plugs into, including the permission broker and safety bounds
- [ADR-0006](../../adr/0006-plan-mode-v2.md) — approval gate + active plan
  path + `<proposed_plan>` rendering
- [ADR-0007](../../adr/0007-plan-progress-panel.md) — sticky progress panel
