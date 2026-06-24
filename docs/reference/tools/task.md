# `task`

`TaskTool` (`crates/neenee-agent/src/task_tool.rs`) is the only tool that
overrides `call_with_events` to stream sub-agent activity back through
`SubTaskEvent`. It is `Read` and overrides `spawns_subagent() = true`, so it is
excluded from every sub-agent profile (recursion guard).

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `description` | string | yes | Max 60 chars |
| `prompt` | string | yes | Sub-agent task |

Spawns a sub-agent that inherits the parent's provider, runs in `AgentMode::Build`,
and receives only the tools admitted by the bound `EXPLORE` profile
(`crates/neenee-core/src/subagent.rs`): `Read` tools that are not
`requires_user()` and not `spawns_subagent()`. Its final answer is returned to
the calling agent, which stays in control of all writes and any user questions.

This page is the parameter reference. The sub-agent mechanism — isolation
model, event streaming, the TUI zoom view, and why the profile excludes
`ask_user` — is explained in [Sub-agents](../../explanation/agent-design/subagents.md).
See also [ADR-0011](../../adr/0011-subagent-profiles.md).
