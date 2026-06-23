# Slash commands

Built-in commands typed in the input box. The descriptions in this table are
the canonical source of truth and match the slash-suggestion popup and the
`/help` output exactly.

Project and user-defined commands are covered under
[Custom commands](#custom-commands).

## Built-in commands

| Command | Description |
|---------|-------------|
| `/provider` | Select an LLM provider |
| `/mode` | Show or switch mode (build, plan) |
| `/mcp` | Show configured MCP server status |
| `/compact` | Compact older complete turns now |
| `/export` | Export the current conversation to the clipboard as Markdown |
| `/clear` | Clear the conversation history |
| `/permissions [clear]` | Show or clear always-allowed tool rules |
| `/session [status\|list\|resume\|fork\|open\|new]` | Manage durable sessions |
| `/sessions` | Browse past sessions |
| `/resume [id]` | Resume the most recent or selected session |
| `/goal` | Set, inspect, complete, or clear the active goal |
| `/loop [objective\|resume\|status\|stop]` | Run an uncapped autonomous goal loop |
| `/init [path]` | Initialize a `.neenee/` config tree |
| `/help` | Show available commands and keybindings |
| `/exit` | Exit the program |

`/provider` and `/exit` are handled entirely in the TUI; the rest are dispatched
by the agent backend.

## Subcommands

### `/mode`

| Form | Effect |
|------|--------|
| `/mode` | Show the current mode |
| `/mode build` | Full read/write tool access |
| `/mode plan` | Read-only tools plus writes under `.neenee/plans/`; the model can also switch modes itself via `plan_enter`/`plan_exit`. See [Plan mode](../explanation/agent-design/plan-mode.md). |
| `/plan` | Open the active plan file in a read-only preview modal. |
| `/verify` | Trigger independent plan verification — spawns a clean-context sub-agent that re-reads the plan and reports PASS/PARTIAL/FAIL per section. |

### `/goal`

| Form | Effect |
|------|--------|
| `/goal` or `/goal status` | Show the current goal, status, and checklist |
| `/goal <objective>` | Set or replace the active goal |
| `/goal edit <objective>` | Rewrite the objective of the current goal |
| `/goal done` | Mark the active goal completed |
| `/goal pause` | Pause an active goal |
| `/goal resume` | Resume a paused, blocked, usage- or budget-limited goal |
| `/goal budget <tokens>` | Set a positive token budget for the goal |
| `/goal budget clear` | Remove the goal's token budget |
| `/goal clear` | Remove the active goal |

Goal state is persisted per session in a SQLite store, so it survives restarts
and is restored on `/resume`. Each turn's token and elapsed-time cost is
accounted against the active goal; reaching the token budget moves the goal to
`budget_limited` until you raise the budget or `/goal resume`.

### `/loop`

| Form | Effect |
|------|--------|
| `/loop` | Start an uncapped autonomous loop on the active goal (set one with `/goal <objective>` first) |
| `/loop <objective>` | Set a fresh goal from `<objective>` and start an uncapped autonomous loop on it |
| `/loop resume` | Resume an unfinished durable checkpoint |
| `/loop status` | Show autonomous loop status |
| `/loop stop` | Stop the active loop |

The loop runs until the model emits `[NEENEE_GOAL_COMPLETE]` (and the goal
checklist allows completion), the user runs `/loop stop` or presses `Esc`, an
error aborts the active turn, or a newer chat or loop request supersedes it.
There is **no iteration budget**: each iteration is a complete agent turn with
its own uncapped ReAct loop, and context compaction keeps long loops bounded.
A legacy `/loop <N>` form (pure number) is rejected with a migration hint.

### `/session`

| Form | Effect |
|------|--------|
| `/session status` | Show session id, parent, counts, checkpoint, compaction |
| `/session list` | List durable session branches |
| `/session resume [id]` | Resume the most recent or selected session |
| `/session fork` | Fork the current conversation into a child session |
| `/session open <id-prefix>` | Open a session by id or id prefix |
| `/session new` | Start a new durable session |

### `/permissions`

| Form | Effect |
|------|--------|
| `/permissions` | List always-allowed tool rules for this process |
| `/permissions clear` | Clear process-local always-allow rules |

### `/init`

| Form | Effect |
|------|--------|
| `/init [path]` | Initialize a `.neenee/` config tree; `path` defaults to `.` |

### `/export`

| Form | Effect |
|------|--------|
| `/export` | Render the live conversation as Markdown — metadata header (session id, provider/model, mode, goal, active plan, exported-at), goal checklist, then a chronological transcript of user prompts, assistant replies, tool calls, and inlined tool results — and copy it to the system clipboard so it can be pasted into another agent to continue the work. |

The receiving agent gets the full chain of decisions and side effects: hidden
and system messages are skipped (mirroring TUI rendering), reasoning traces
are folded into collapsible `<details>` blocks, and sub-agent transcripts
nested under `task` results are summarised by message counts instead of
dumped in full. If the system clipboard is unavailable, the export falls
back to OSC52 or surfaces the underlying clipboard error.

## Custom commands

Markdown files discovered in `.neenee/commands/` (project-local, higher
priority), `$XDG_DATA_HOME/neenee/commands/` (user-global, XDG; default
`~/.local/share/neenee/commands/`), and `~/.neenee/commands/` (legacy
pre-XDG fallback, emits a deprecation warning — see ADR-0013). The filename
stem or frontmatter `name` becomes `/name` after lowercasing and stripping a
leading `/`. Names allow ASCII letters, digits, `-`, and `_`.

Optional YAML frontmatter:

```yaml
---
name: review
description: Review changes
---
```

The template body supports `$ARGUMENTS` (the full argument string) and `$1`
through `$9` positional placeholders. Built-in command names are reserved and
are not shadowed by custom commands.

## See also

- [Harness architecture](../explanation/agent-design/harness.md) — goal state, autonomous
  loop, durable session, permission broker, context compaction
- [Modals](tui/modals.md) — the `/provider` and `/sessions` pickers
