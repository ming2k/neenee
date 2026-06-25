# Built-in tools

The neenee agent exposes a fixed set of built-in tools to the model on every
turn. MCP server tools are appended at runtime. This is the lookup
surface — one page per tool category. For how tools are gated (access tiers,
capability axes, the permission broker), see [Tool access](access.md).

Most built-in tools live in the `neenee-tools` crate; `subagent` and `use_skill`
live in `neenee-agent`. The `Tool` trait is defined in
`crates/neenee-core/src/capability.rs`.

## Registry

Registration order is the literal in `crates/neenee-cli/src/main.rs`.
`Agent::new` (`crates/neenee-agent/src/agent.rs`) appends the `todo` /
`todo_update` tools so they share the agent's live task-list cell.
`SubagentTool` is pushed last so it can capture a snapshot of the assembled
toolset.

The pursuit lifecycle has no model-facing tools: `/pursue` (entry, user), the
stop-gate (continuation, harness), and `[NEENEE_PURSUIT_COMPLETE]` (exit, model)
own the three phases directly. See [pursuits](pursuits.md) and ADR-0031.

| Tool | Access | Permission scope | Reference page |
|------|--------|------------------|----------------|
| `bash` | `Execute` | `command` argument | [bash](bash.md) |
| `abort` | `Read` (control-flow) | `*` | [abort](abort.md) |
| `read_file` | `Read` | `*` | [filesystem](filesystem.md) |
| `read_image` | `Read` | `*` | [filesystem](filesystem.md) |
| `write_file` | `Write` | `path` argument | [filesystem](filesystem.md) |
| `edit_file` | `Write` | `path` argument | [filesystem](filesystem.md) |
| `grep` | `Read` | `*` | [filesystem](filesystem.md) |
| `glob` | `Read` | `*` | [filesystem](filesystem.md) |
| `list_dir` | `Read` | `*` | [filesystem](filesystem.md) |
| `ask_user` | `Read` | `*` | [interaction](interaction.md) |
| `todo` | `Read` | `*` | [interaction](interaction.md) |
| `todo_update` | `Read` | `*` | [interaction](interaction.md) |
| `webfetch` | `Read` | `*` | [web](web.md) |
| `websearch` | `Read` | `*` | [web](web.md) |
| `subagent` | `Read` (spawns subagent) | `*` | [subagent](subagent.md) |
| `search_history` | `Read` | `*` | [skills](skills.md) |
| `use_skill` | `Read` | `*` | [skills](skills.md) |
| `list_skills` | `Read` | `*` | [skills](skills.md) |
| `reload_skills` | `Read` | `*` | [skills](skills.md) |
| `create_project` | `Write` | `{path}/{name}` or `*` | [projects](projects.md) |
| `init_config` | `Write` | `path` argument or `.` | [projects](projects.md) |
| `mcp__<server>__<tool>` | `Read` if server `read_only = true`, else `Write` | `*` | [mcp](mcp.md) |

`permission_scope` defaults to `"*"`. Only `write_file`, `edit_file`, `bash`,
`create_project`, and `init_config` override it; their scope string is what a
cached `Always` rule matches against.

Parameters are exposed to the model as JSON Schema via
`Tool::to_openai_function()` (`crates/neenee-core/src/capability.rs`), which
wraps `Tool::parameters()`.

## See also

- [Tool access](access.md) — access tiers, capability axes, permission broker
- [How to add a tool](../../how-to/add-a-tool.md) — implementing the `Tool` trait
- [Tool rounds](../../explanation/agent-design/turns-and-rounds.md) — how schemas are
  injected, streamed, and fell back to text
