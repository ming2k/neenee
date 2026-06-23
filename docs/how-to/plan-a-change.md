# How to plan a change before implementing

This guide shows how to use **Plan mode** to research and design a change
before any workspace edits happen. For the design rationale, see
[Plan mode](../explanation/agent-design/plan-mode.md). For the mode command and the
planning tools, see [Slash commands](../reference/commands.md) and
[Built-in tools](../reference/tools/index.md).

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

## Track progress

Once the plan is approved a sticky 3-row panel appears above the input
box showing the section completion ratio and per-section status:

```text
╭── Plan: rewrite-auth.md · 1/4 done ───────────────╮
│ ✓ Summary  ● Key Changes  ○ Test Plan  ○ Assump… │
╰───────────────────────────────────────────────────╯
```

The model marks sections `in_progress` → `done` via the
`update_plan_progress` tool as it works. Sections it has not touched yet
show as `○ Pending` — that is honest, not stale: it means "not verified
yet." If you see the panel stay on `○` for a section the model claims is
done, ask it to mark the section or to run the verifier.

The panel disappears when:

- the agent re-enters Plan mode (a new planning cycle starts), or
- the user runs `/mode plan` manually, or
- the session is resumed after both were cleared.

## Stay in control

- `/mode` shows the current mode at any time.
- `/mode build` is the authoritative override that ends planning immediately,
  even if the agent entered Plan mode on its own.
- Plan mode is orthogonal to [pursuit state](../explanation/agent-design/harness.md) and the
  autonomous loop; a pursuit can be active in either mode.

## See also

- [Plan mode](../explanation/agent-design/plan-mode.md) — why the mode exists and how the
  write exemption works
- [Slash commands](../reference/commands.md) — the `/mode` command
- [Built-in tools](../reference/tools/index.md) — `plan_enter` and `plan_exit`
