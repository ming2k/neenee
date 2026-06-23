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
| `/pursue [condition\|status\|stop\|done\|edit\|clear]` | Pursue a pursuit: the harness keeps the turn going until the condition is met |
| `/repeat [cron prompt\|list\|cancel id]` | Schedule a prompt on a cron expression |
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

### `/pursue`

| Form | Effect |
|------|--------|
| `/pursue <condition>` | Set the condition as the active pursuit, arm the stop-gate, and drive the turn until it is met |
| `/pursue` | Re-arm and drive a pursuit on the existing active pursuit |
| `/pursue status` | Show the current pursuit, armed state, and gate iteration |
| `/pursue edit <condition>` | Rewrite the condition of the current pursuit |
| `/pursue done` | Mark the pursuit completed (disarms the gate) |
| `/pursue stop` | Stop the active pursuit |
| `/pursue clear` | Remove the pursuit (disarms and clears) |

`/pursue` arms a **stop-gate**: each time the model would end the turn, the
harness re-injects the condition and forces another round until the model
signals completion (`[NEENEE_PURSUIT_COMPLETE]`), the 50-round safety cap is hit,
or the user interrupts (`/pursue stop` / `Esc`). Pursuit state is persisted per
session in SQLite, so it survives restarts and is restored on `/resume`. See
[Pursuits and the pursue stop-gate](../explanation/agent-design/pursuits.md).

### `/repeat`

| Form | Effect |
|------|--------|
| `/repeat <cron> <prompt>` | Schedule `<prompt>` on the five-field `<cron>` and run it now |
| `/repeat list` | List scheduled jobs (id, cron, next fire, prompt) |
| `/repeat cancel <id>` | Cancel a scheduled job |
| `/repeat help` | Show cron syntax help |

`<cron>` is five fields — `minute hour day-of-month month day-of-week` — e.g.
`*/5 * * * *` (every 5 minutes), `0 9 * * 1-5` (09:00 on weekdays). Jobs are
durable (survive restarts) and auto-expire after 30 days. `/repeat` is a
clock-driven scheduler, independent of `/pursue`. See
[Pursuits and the pursue stop-gate](../explanation/agent-design/pursuits.md).

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
| `/export` | Render the live conversation as Markdown — metadata header (session id, provider/model, mode, pursuit, active plan, exported-at), pursuit checklist, then a chronological transcript of user prompts, assistant replies, tool calls, and inlined tool results — and copy it to the system clipboard so it can be pasted into another agent to continue the work. |

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
pre-XDG fallback, emits a deprecation warning — see
[ADR-0013](../adr/0013-skills-xdg-paths-and-bundled-embed.md)). The
filename stem or frontmatter `name` becomes `/name` after lowercasing and
stripping a leading `/`. Names allow ASCII letters, digits, `-`, and `_`.

See [Paths](paths.md) for the full override stack and the project-vs-XDG
boundary.

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

- [Harness architecture](../explanation/agent-design/harness.md) — pursuit state, autonomous
  loop, durable session, permission broker, context compaction
- [Modals](tui/modals.md) — the `/provider` and `/sessions` pickers
