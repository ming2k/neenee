# Slash commands

Built-in commands typed in the input box. The descriptions in this table are
the canonical source of truth and match the slash-suggestion popup and the
`/help` output exactly.

Project and user-defined commands are covered under
[Custom commands](#custom-commands).

## Built-in commands

| Command | Description |
|---------|-------------|
| `/models` | Select an LLM provider |
| `/mode` | Show or switch mode (build, plan) |
| `/mcp` | Show configured MCP server status |
| `/compact` | Compact older complete turns now |
| `/clear` | Clear the conversation history |
| `/permissions [clear]` | Show or clear always-allowed tool rules |
| `/session [status\|list\|resume\|fork\|open\|new]` | Manage durable sessions |
| `/sessions` | Browse past sessions |
| `/resume [id]` | Resume the most recent or selected session |
| `/goal` | Set, inspect, complete, or clear the active goal |
| `/loop [N\|resume\|status\|stop]` | Run or resume bounded autonomous goal work |
| `/init [path]` | Initialize a `.neenee/` config tree |
| `/help` | Show available commands and keybindings |
| `/exit` | Exit the program |

`/models` and `/exit` are handled entirely in the TUI; the rest are dispatched
by the agent backend.

## Subcommands

### `/mode`

| Form | Effect |
|------|--------|
| `/mode` | Show the current mode |
| `/mode build` | Full read/write tool access |
| `/mode plan` | Read-only tools, safe exploration |

### `/goal`

| Form | Effect |
|------|--------|
| `/goal` or `/goal status` | Show the current goal and checklist |
| `/goal <objective>` | Set or replace the active goal |
| `/goal done` | Mark the active goal completed |
| `/goal clear` | Remove the active goal |

### `/loop`

| Form | Effect |
|------|--------|
| `/loop <N>` | Run up to N autonomous iterations; `N` is `1..=50`, default 8 |
| `/loop resume` | Resume an unfinished durable checkpoint |
| `/loop status` | Show autonomous loop status |
| `/loop stop` | Stop the active loop |

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

## Custom commands

Markdown files discovered in `.neenee/commands/` (project-local, higher
priority) and `~/.neenee/commands/` (user-global, fallback). The filename stem
or frontmatter `name` becomes `/name` after lowercasing and stripping a leading
`/`. Names allow ASCII letters, digits, `-`, and `_`.

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

- [Harness architecture](../explanation/harness.md) — goal state, autonomous
  loop, durable session, permission broker, context compaction
- [Modals](tui/modals.md) — the `/models` and `/sessions` pickers
