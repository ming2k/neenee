# Built-in tools

The neenee agent exposes a fixed set of built-in tools to the model on every
turn. MCP server tools and the synthetic `goal_checklist` tool are appended at
runtime. This page lists every tool name, its access class, parameter schema,
permission scope, and source location.

All production tools live in the `neenee-core` crate. The `Tool` trait is
defined at `crates/neenee-core/src/lib.rs:156`.

## Tool access

`ToolAccess` (`crates/neenee-core/src/lib.rs:195`) gates two surfaces:

| Variant | Plan mode | Permission broker |
|---------|-----------|-------------------|
| `ReadOnly` | Allowed | Bypassed |
| `Write` (default) | Blocked with an error | Prompted unless a cached `Always` rule matches |

A tool that does not override `access()` is treated as `Write`. The Plan-mode
gate at `crates/neenee-core/src/lib.rs:976` and the permission broker at
`crates/neenee-core/src/lib.rs:983` both consult the same enum, so a tool
marked `ReadOnly` is trusted in both surfaces.

## Built-in tool registry

Registration order is the literal in `crates/neenee/src/main.rs:899-915`.
`Agent::new` strips any external `goal_checklist` and appends its own
(`crates/neenee-core/src/lib.rs:442-443`). `TaskTool` is pushed last so it can
capture a snapshot of the assembled toolset.

| Tool name | Access | Permission scope | Source |
|-----------|--------|------------------|--------|
| `bash` | `Write` | `command` argument | `crates/neenee-core/src/tools.rs:292` |
| `read_file` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:121` |
| `write_file` | `Write` | `path` argument | `crates/neenee-core/src/tools.rs:184` |
| `edit_file` | `Write` | `path` argument | `crates/neenee-core/src/tools.rs:229` |
| `grep` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:367` |
| `glob` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:606` |
| `list_dir` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:428` |
| `webfetch` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:763` |
| `websearch` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:940` |
| `todo` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:1043` |
| `create_project` | `Write` | `{path}/{name}` or `*` | `crates/neenee-core/src/project.rs:22` |
| `init_config` | `Write` | `path` argument or `.` | `crates/neenee-core/src/project.rs:116` |
| `use_skill` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:536` |
| `task` | `ReadOnly` | `*` | `crates/neenee-core/src/tools.rs:1167` |
| `goal_checklist` | `ReadOnly` | `*` (no permission prompt) | `crates/neenee-core/src/tools.rs:18` |
| `mcp__<server>__<tool>` | `ReadOnly` if server `read_only = true`, else `Write` | `*` | `crates/neenee-core/src/mcp.rs:230` |

`permission_scope` defaults to `"*"` (`crates/neenee-core/src/lib.rs:163`).
Only `write_file`, `edit_file`, `bash`, `create_project`, and `init_config`
override it; their scope string is what a cached `Always` rule matches
against.

## Parameters

Parameters are exposed to the model as JSON Schema via
`Tool::to_openai_function()` (`crates/neenee-core/src/lib.rs:183`), which
wraps `Tool::parameters()`.

### `bash`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `command` | string | yes | — | Shell command line |
| `timeout` | integer | no | `30` | Seconds |

### `read_file`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | File path |
| `offset` | integer | no | — | 1-based start line |
| `limit` | integer | no | — | Max lines |

### `write_file`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `path` | string | yes | File path |
| `content` | string | yes | Full content; overwrites |

### `edit_file`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `path` | string | yes | File path |
| `old_string` | string | yes | Must exist verbatim |
| `new_string` | string | yes | Replacement text |

### `grep`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Regex |
| `path` | string | no | `.` | Search root |
| `ext` | string | no | — | File extension filter |

Backed by ripgrep.

### `glob`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Glob, e.g. `**/*.rs` |
| `path` | string | no | `.` | Search root |

Capped at `GLOB_MAX_RESULTS = 200` (`crates/neenee-core/src/tools.rs:583`).

### `list_dir`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Directory |
| `pattern` | string | no | — | Optional glob |
| `recursive` | boolean | no | `false` | Recurse |
| `max_results` | integer | no | `100` | Cap |

### `webfetch`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `url` | string | yes | — | `http` or `https` |
| `raw` | boolean | no | `false` | Skip HTML-to-text |

### `websearch`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `query` | string | yes | DuckDuckGo query; no API key |

### `todo`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `items` | array | yes | Max 50 items |

Each item: `{ "content": string, "status": "pending" | "in_progress" | "completed" }`.

### `task`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `description` | string | yes | Max 60 chars |
| `prompt` | string | yes | Sub-agent task |

Spawns a read-only sub-agent. See [Special tools](#special-tools).

### `goal_checklist`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `items` | array | yes | Max 50 items |

Each item: `{ "content": string, "status": "pending" | "in_progress" | "completed" | "cancelled" }`.

Hard rules enforced at `crates/neenee-core/src/tools.rs:66-97`:

- Empty `content` rejected.
- At most one `in_progress` item.
- A non-empty checklist cannot be replaced with an empty list; each item must
  receive a terminal `completed` or `cancelled` status.

### `use_skill`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `name` | string | yes | Skill name from frontmatter |

Loads the skill body into the conversation. Skill discovery is documented in
[Skills](#skills).

### `create_project`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `name` | string | yes | — | Project name |
| `type` | enum | yes | — | `rust`, `node`, `python`, `go`, `generic` |
| `path` | string | no | `.` | Parent directory |
| `git` | boolean | no | `true` | `git init` |
| `neenee` | boolean | no | `false` | Scaffold `.neenee/` |

### `init_config`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Target directory |

Idempotent; existing files are never overwritten.

### `mcp__<server>__<tool>`

Parameters come from the MCP server's `inputSchema`, falling back to
`{"type":"object"}` when absent (`crates/neenee-core/src/mcp.rs:199-202`). The
public name is `mcp__{sanitized_server}__{sanitized_original}`
(`crates/neenee-core/src/mcp.rs:189-193`).

## Special tools

### `goal_checklist`

`GoalChecklistTool` (`crates/neenee-core/src/tools.rs:18`) is force-injected
by `Agent::new` and shares the agent's `Arc<Mutex<Option<Goal>>>`. Updates
write directly into live goal state and surface as
`AgentResponse::HarnessState` to the TUI. It is `ReadOnly` and bypasses the
permission broker entirely.

### `task`

`TaskTool` (`crates/neenee-core/src/tools.rs:1167`) is the only tool that
overrides `call_with_events` (`tools.rs:1194`) to stream sub-agent activity
back through `SubTaskEvent`. The sub-agent:

- Inherits the parent's provider (`tools.rs:1232-1233`).
- Runs in `AgentMode::Build` (`tools.rs:1233`).
- Receives only tools where `access() == ReadOnly && name != "task"`
  (`tools.rs:1228`), preventing recursion and any write capability.
- Uses a forced read-only system prompt (`tools.rs:1235-1241`).

### MCP tools

Each MCP server's tools are wrapped in `McpTool`
(`crates/neenee-core/src/mcp.rs:230`) and dispatch `tools/call` JSON-RPC over
stdio to the server child process. The wrapper inherits the server's
`read_only` flag as its `ToolAccess` (`mcp.rs:208-212`). Connect and
`tools/list` are bounded by `MCP_CONNECT_TIMEOUT = 8s` (`mcp.rs:23, 277-281`).
Configuration lives in `config.toml` under `[mcp.<server>]`.

## Skills

Skills are not tools, but the `use_skill` tool loads them. A skill is a
Markdown file with optional YAML frontmatter:

```text
---
name: my-skill
description: When to invoke this skill
---
Skill body injected into the system prompt on demand.
```

Discovery (`crates/neenee-core/src/skills.rs:40`) searches, in order:

1. `.neenee/skills/` in the project root.
2. `~/.neenee/skills/` in the user home.

First-seen wins; project-local overrides user-global. The skill index is
injected into the system prompt at `crates/neenee-core/src/lib.rs:612-615`.

## See also

- [Tool protocol](../explanation/tool-protocol.md) — how schemas are injected,
  streamed, and fell back to text
- [Provider capabilities](../explanation/provider-capabilities.md) — which
  providers support native function calling
- [How to add a tool](../how-to/add-a-tool.md) — implementing the `Tool` trait
- [Harness architecture](../explanation/harness.md) — control plane around
  tool execution
