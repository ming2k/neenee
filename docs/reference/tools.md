# Built-in tools

The neenee agent exposes a fixed set of built-in tools to the model on every
turn. MCP server tools and the synthetic goal tools (`get_goal`, `create_goal`,
`update_goal`, `goal_checklist`) are appended at runtime. This page lists every
tool name, its access class, parameter schema, permission scope, and source
location.

All production tools live in the `neenee-core` crate. The `Tool` trait is
defined in `crates/neenee-core/src/lib.rs`.

## Tool access

`ToolAccess` (`crates/neenee-core/src/lib.rs`) gates two surfaces:

| Variant | Plan mode | Permission broker |
|---------|-----------|-------------------|
| `Read` | Allowed | Bypassed |
| `Write` (default) | Blocked unless `allowed_in_plan_mode` exempts the call | Prompted unless a cached `Always` rule matches |

A tool that does not override `access()` is treated as `Write`. Plan mode is
enforced per-invocation through `Tool::allowed_in_plan_mode(arguments)`, which
defaults to `access() == Read`; `write_file` and `edit_file` override it to
also permit writes under `.neenee/plans/`. The Plan-mode gate in
`Agent::execute_tool` and the permission broker both consult `ToolAccess`, so a
tool marked `Read` is trusted in both surfaces. See
[Plan mode](../explanation/plan-mode.md) for the exemption rationale.

## Built-in tool registry

Registration order is the literal in `crates/neenee/src/main.rs`. `Agent::new`
strips any externally supplied `goal_checklist`, `get_goal`, `create_goal`,
`update_goal`, `plan_enter`, and `plan_exit`, then appends its own goal tools
from `crates/neenee-core/src/goals/tools.rs` and plan tools from
`crates/neenee-core/src/plan.rs` so they share the agent's live state.
`TaskTool` is pushed last so it can capture a snapshot of the assembled toolset.

| Tool name | Access | Permission scope | Source |
|-----------|--------|------------------|--------|
| `bash` | `Write` | `command` argument | `crates/neenee-core/src/tools.rs` |
| `read_file` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `ask_user` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `write_file` | `Write` (Plan-exempt under `.neenee/plans/`) | `path` argument | `crates/neenee-core/src/tools.rs` |
| `edit_file` | `Write` (Plan-exempt under `.neenee/plans/`) | `path` argument | `crates/neenee-core/src/tools.rs` |
| `grep` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `glob` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `list_dir` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `webfetch` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `websearch` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `todo` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `create_project` | `Write` | `{path}/{name}` or `*` | `crates/neenee-core/src/project.rs` |
| `init_config` | `Write` | `path` argument or `.` | `crates/neenee-core/src/project.rs` |
| `use_skill` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `task` | `Read` | `*` | `crates/neenee-core/src/tools.rs` |
| `get_goal` | `Read` | `*` | `crates/neenee-core/src/goals/tools.rs` |
| `create_goal` | `Write` | `*` | `crates/neenee-core/src/goals/tools.rs` |
| `update_goal` | `Write` | `*` | `crates/neenee-core/src/goals/tools.rs` |
| `goal_checklist` | `Read` | `*` (no permission prompt) | `crates/neenee-core/src/goals/tools.rs` |
| `plan_enter` | `Read` | `*` | `crates/neenee-core/src/plan.rs` |
| `plan_exit` | `Read` | `*` | `crates/neenee-core/src/plan.rs` |
| `mcp__<server>__<tool>` | `Read` if server `read_only = true`, else `Write` | `*` | `crates/neenee-core/src/mcp.rs` |

`permission_scope` defaults to `"*"`. Only `write_file`, `edit_file`, `bash`,
`create_project`, and `init_config` override it; their scope string is what a
cached `Always` rule matches against.

## Parameters

Parameters are exposed to the model as JSON Schema via
`Tool::to_openai_function()` (`crates/neenee-core/src/lib.rs`), which
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

### `ask_user`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `questions` | array | yes | 1–5 questions |

Each question: `{ "header"?: string, "question": string, "options": Option[], "multi_select"?: boolean }`.
Each option: `{ "label": string, "description"?: string }`. The model should put the recommended option first and suffix its label with `(Recommended)`. The TUI always appends an "Other" free-text option so the user is not forced into the model's choices.

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

Capped at `GLOB_MAX_RESULTS = 200` (`crates/neenee-core/src/tools.rs`).

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

### `get_goal`

No parameters. Returns the current goal as JSON, including status, token budget,
`tokens_used`, elapsed time, and remaining budget, or `{"goal": null}` when none
is set.

### `create_goal`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `objective` | string | yes | The objective to start pursuing |
| `token_budget` | integer | no | Positive budget; omit unless explicitly requested |

Starts a new active goal. The model is instructed to call this only when the
user or developer instructions explicitly ask for a goal, never inferred from an
ordinary task.

### `update_goal`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `status` | enum | yes | `complete` or `blocked` |

Marks the active goal `complete` (objective achieved, no work remaining) or
`blocked` (same blocking condition recurred across consecutive goal turns).
Pause, resume, and budget/usage limits are user-controlled, not reachable here.

### `goal_checklist`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `items` | array | yes | Max 50 items |

Each item: `{ "content": string, "status": "pending" | "in_progress" | "completed" | "cancelled" }`.

Hard rules enforced in `crates/neenee-core/src/goals/tools.rs`:

- Empty `content` rejected.
- At most one `in_progress` item.
- A non-empty checklist cannot be replaced with an empty list; each item must
  receive a terminal `completed` or `cancelled` status.

### `plan_enter`

No parameters. Switches the agent to `Plan` mode. The model calls it when a
request would benefit from designing before implementing; it should not be
called for simple tasks or when the user wants immediate implementation. See
[Plan mode](../explanation/plan-mode.md).

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
[Plan mode](../explanation/plan-mode.md).

### `update_plan_progress`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `section` | string | yes | Substring of the `##` heading to update (case-insensitive) |
| `status` | enum: `pending` / `in_progress` / `done` / `skipped` | yes | New status for the section |

Mark a section of the active plan. The agent calls this as it works through
the implementation so the sticky panel above the input box reflects the
current state. The section argument is matched case-insensitively as a
substring of any `##` heading, so the model does not have to echo the exact
title. Has no effect if there is no active plan (the call returns a clear
"no active plan" hint instead of erroring). See
[Plan mode](../explanation/plan-mode.md).

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
`{"type":"object"}` when absent (`crates/neenee-core/src/mcp.rs`). The
public name is `mcp__{sanitized_server}__{sanitized_original}`.

## Special tools

### Goal tools

`GetGoalTool`, `CreateGoalTool`, `UpdateGoalTool`, and `GoalChecklistTool`
(`crates/neenee-core/src/goals/tools.rs`) are force-injected by `Agent::new`.
They share a `GoalToolContext` carrying the session/thread id and the
`GoalService`, which persists goal state in SQLite; `goal_checklist` also shares
the agent's live `Arc<Mutex<Option<Goal>>>`. `get_goal` and `update_goal`/
`create_goal` read and mutate the persisted goal; `goal_checklist` writes
directly into live goal state and surfaces as `AgentResponse::GoalUpdated` to the
TUI. `get_goal` and `goal_checklist` are `Read` and bypass the permission
broker; `create_goal` and `update_goal` are `Write`.

### `task`

`TaskTool` (`crates/neenee-core/src/tools.rs`) is the only tool that
overrides `call_with_events` to stream sub-agent activity
back through `SubTaskEvent`. The sub-agent:

- Inherits the parent's provider.
- Runs in `AgentMode::Build`.
- Receives only tools where `access() == Read && name != "task"`,
  preventing recursion and any write capability.
- Uses a forced read-only system prompt.

### Plan tools

`PlanEnterTool` and `PlanExitTool` (`crates/neenee-core/src/plan.rs`) are
force-injected by `Agent::new`. They share a `PlanToolContext` carrying the
same `Arc<Mutex<AgentMode>>` the `Agent` owns, so each tool flips the mode in
place. Both are `Read` and bypass the permission broker. After either
returns, the agent emits a `ModeChanged` event so the TUI refreshes its mode
indicator. The Plan-mode gate exempts `.neenee/plans/` writes through
`Tool::allowed_in_plan_mode`; see [Plan mode](../explanation/plan-mode.md).

### MCP tools

Each MCP server's tools are wrapped in `McpTool`
(`crates/neenee-core/src/mcp.rs`) and dispatch `tools/call` JSON-RPC over
stdio to the server child process. The wrapper inherits the server's
`read_only` flag as its `ToolAccess`. Connect and `tools/list` are bounded
by `MCP_CONNECT_TIMEOUT = 8s`. Configuration lives in `config.toml` under
`[mcp.<server>]`.

## Skills

Skills are not tools, but the `use_skill` tool loads them. A skill is a
Markdown file with YAML frontmatter, conventionally named `SKILL.md` inside a
skill directory:

```text
.neenee/skills/<name>/SKILL.md
~/.neenee/skills/<name>/SKILL.md
```

```text
---
name: my-skill
description: When to invoke this skill
short-description: Short help
version: "1.0.0"
tags: [rust]
policy:
  allow_implicit_invocation: true
dependencies:
  tools:
    - type: mcp
      value: rust-analyzer
---
Skill body injected into the context on demand.
```

The skill registry (`crates/neenee-core/src/skills/`) discovers skills from,
in priority order (later sources override earlier ones):

1. Bundled system skills (`~/.neenee/skills/.system/`).
2. Remote skill repositories configured in `config.toml`, cached under
   `~/.cache/neenee/skills/`.
3. User-global skills: `~/.neenee/skills/`, `~/.agents/skills/`,
   `~/.claude/skills/`, `~/.kimi-code/skills/`.
4. Extra local paths configured in `config.toml`.
5. Project-repo skills: `.neenee/skills/`, `.agents/skills/`, `.claude/skills/`,
   `.kimi-code/skills/`.

The catalog is built by `build_skills_index` and injected into the system
prompt by `Agent::build_system_prompt`. Skills whose names are mentioned in a
user message are auto-loaded by `Agent::inject_implicit_skills` when their
policy allows implicit invocation.

## See also

- [Tool rounds](../explanation/tool-rounds.md) — how schemas are injected,
  streamed, and fell back to text
- [Provider capabilities](../explanation/provider-capabilities.md) — which
  providers support native function calling
- [How to add a tool](../how-to/add-a-tool.md) — implementing the `Tool` trait
- [Harness architecture](../explanation/harness.md) — control plane around
  tool execution
