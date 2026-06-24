# Plan mode

Plan mode is a read-only execution surface for researching and designing a
change before any workspace edits happen. It exists so a complex request can be
investigated and written up as a plan without the model mutating source files
halfway through understanding them.

Plan mode is one of two **agent modes**. The mode is a property of the agent's
control plane, not of a provider or a session, and it gates tool execution on
every research-and-act round.

## The two modes

There are two modes, each with a different tool surface:

| Mode | Tool surface | Entered by |
|------|--------------|------------|
| Build | Every tool, subject to the permission broker | Default; `/mode build`; `plan_exit` |
| Plan | Read-only tools plus writes under `.neenee/plans/` only | `/mode plan`; `plan_enter` |

The active mode is shown in the TUI header accent color and can be switched
manually with the [`/mode`](../../reference/commands.md) command at any time.

## Entering and exiting

There are two independent ways to switch mode. Either one updates the same
shared mode state, so they never disagree.

### Manual: `/mode`

The user runs `/mode plan` or `/mode build` directly. This is the authoritative
override: it works regardless of what the model is doing and is how a user
forces the agent back into Build mode if planning has gone far enough.

### Autonomous: `plan_enter` and `plan_exit`

The model can switch modes itself through two built-in tools:

| Tool | Access | Effect |
|------|--------|--------|
| `plan_enter` | Read | Switch to Plan mode |
| `plan_exit` | Read | Switch back to Build mode; optional `plan_path` records the plan file |

The system prompt describes the workflow: when a request is complex, spans
multiple files, or would benefit from designing first, the model calls
`plan_enter`; it researches with read-only tools, writes the plan to
`.neenee/plans/<name>.md`, then calls `plan_exit` to return to Build mode and
implement the plan.

The switch is automatic and unconditional; neenee does not prompt the user to
confirm an autonomous mode change. The user stays in control through `/mode`
and can revert at any time. See [Built-in tools](../../reference/tools/index.md) for
the parameter schemas.

## The plan-file write exemption

Plan mode is read-only, with one deliberate exception. The file write tools may
write files under `.neenee/plans/` while planning, so the model can persist the
plan document it is supposed to produce. Every other write target remains
blocked.

The exemption is decided **per invocation, not per tool**. Each write tool,
when it runs, checks the resolved target path and is allowed only when that
path sits inside the plans directory. Path resolution canonicalizes the parent
directory and re-appends the file name, so a brand-new plan file that does not
exist yet still resolves correctly. The exemption only relaxes the Plan-mode
gate; the permission broker still applies, so a plan-file write still follows
the normal once/always/reject flow.

## How a mode switch propagates

A mode change takes effect immediately and is visible everywhere that reads
mode state:

- The plan tools and the agent share one mode cell, so `plan_enter` and
  `plan_exit` mutate the value everyone else reads.
- The system prompt is rebuilt before every model round, so the round after a
  switch sees the correct mode description and tool restrictions.
- After `plan_enter` or `plan_exit` returns, the agent emits a `ModeChanged`
  event, which is relayed to the TUI so the header indicator refreshes live,
  mid-turn.

Because the gate is re-evaluated on every tool call, a single turn can cross
the boundary: the model calls `plan_enter`, researches in Plan mode, writes the
plan, then calls `plan_exit` and continues implementing in Build mode, all
within one agent run.

## Relationship to pursuits and the autonomous loop

Plan mode is orthogonal to [pursuit state](harness.md) and the
[autonomous loop](harness.md). A pursuit and a loop can be active in either mode.
In practice the model usually enters Plan mode first, exits to Build once the
plan is written, and then pursues the pursuit with full tool access. The mode does
not affect pursuit accounting, the completion marker, or the autonomous loop.

## Plan progress and the task list

Once `plan_exit` is approved the agent seeds the **unified task list** from the
plan's `##` headings — one `pending` item per section. The list is the single
source of truth for "what is left to do," shared with the `todo` /
`todo_update` tools, shown in the Activity modal, and persisted across
restarts. Entering Plan mode (via `plan_enter` or `/mode plan`) clears it,
since a fresh planning cycle invalidates the previous plan's steps.

Collapsed in the Activity modal, the Tasks section shows a done/total ratio;
expanded, it lists every step in file order with a status glyph — `✓`
completed, `●` in progress, `○` pending, `✕` cancelled — so the whole plan is
readable at a glance without opening the file. The list is hidden when no
items exist.

Step status is **model-driven, not inferred**. The system prompt instructs the
model to move a step to `in_progress` when it starts and `completed` when it is
done, using the `todo` (full-replace) or `todo_update` (mark one step) tools. A
step the model forgets to mark stays pending — which is honest (the work has
not been verified) rather than a stale auto-progress that misleads. A step can
also be marked `cancelled` when it turns out not to apply.

Because status depends on the model remembering to report it, the panel also
watches for silence: if several turns pass without any update, the header dims
to flag that the checks may no longer reflect reality. The list is persisted
across restarts, so `/resume` restores it in the same state. See
[ADR-0020](../../adr/0020-unified-task-list.md) for the design rationale (this
supersedes the per-plan progress panel of [ADR-0007](../../adr/0007-plan-progress-panel.md)).

## See also

- [Built-in tools](../../reference/tools/index.md) — `plan_enter`, `plan_exit`,
  `todo`, and `todo_update` parameter schemas, and the per-invocation plan-path
  access rule
- [Slash commands](../../reference/commands.md) — the `/mode` command
- [Harness architecture](harness.md) — the control plane that Plan mode plugs
  into, including the permission broker and safety bounds
- [ADR-0006](../../adr/0006-plan-mode-v2.md) — approval gate, active plan
  path, and proposed-plan rendering
- [ADR-0020](../../adr/0020-unified-task-list.md) — unified task list
  (supersedes ADR-0007's per-plan progress panel)
