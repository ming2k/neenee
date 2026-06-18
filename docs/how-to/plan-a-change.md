# How to plan a change before implementing

This guide shows how to use **Plan mode** to research and design a change
before any workspace edits happen. For the design rationale, see
[Plan mode](../explanation/plan-mode.md). For the mode command and the
planning tools, see [Slash commands](../reference/commands.md) and
[Built-in tools](../reference/tools.md).

## Pick who starts the plan

Plan mode can be entered two ways; choose based on how much direction you want
to give up front.

- Let the agent decide: describe the task normally. When the work is complex,
  multi-file, or architectural, the agent calls `plan_enter` itself, researches,
  writes the plan, then calls `plan_exit` and implements it.
- Force it yourself: run `/mode plan` before sending the task. Use this when
  you want only research and a written plan, with no edits attempted yet.

## Enter Plan mode

Run:

```text
/mode plan
```

The header accent color changes to indicate Plan mode. In this mode the agent
can use read-only tools freely and can write files only under `.neenee/plans/`.
Any other write is blocked and returned to the model as an error.

## Research and write the plan

Send the task. Let the agent explore the codebase with read-only tools
(`read_file`, `grep`, `glob`, `list_dir`, `task`). Ask it to write the plan to
a file under the plans directory, for example:

```text
.neenee/plans/rewrite-auth.md
```

That path is the only writable target in Plan mode, so the plan document can be
persisted without unlocking the rest of the workspace. The write still goes
through the normal permission flow, so approve it once or always.

## Exit Plan mode and implement

When the plan is complete, switch back to full tool access:

```text
/mode build
```

or let the agent call `plan_exit` itself. From here the agent edits, runs
builds, and verifies the work normally. To resume editing immediately from a
written plan, point the agent at the plan file, for example:

```text
Implement the plan at .neenee/plans/rewrite-auth.md
```

## Stay in control

- `/mode` shows the current mode at any time.
- `/mode build` is the authoritative override that ends planning immediately,
  even if the agent entered Plan mode on its own.
- Plan mode is orthogonal to [goal state](../explanation/harness.md) and the
  autonomous loop; a goal and `/loop` can be active in either mode.

## See also

- [Plan mode](../explanation/plan-mode.md) — why the mode exists and how the
  write exemption works
- [Slash commands](../reference/commands.md) — the `/mode` command
- [Built-in tools](../reference/tools.md) — `plan_enter` and `plan_exit`
