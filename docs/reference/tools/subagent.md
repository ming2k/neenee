# `subagent`

`SubagentTool` (`crates/neenee-agent/src/subagent_tool.rs`) is the dispatch
tool that spawns a research subagent. It overrides `call_structured_with_events`
to stream subagent activity back through `SubagentEvent`, and is `Read` with
`spawns_subagent() = true`, so every subagent profile excludes it (recursion
guard).

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `description` | string | yes | Max 60 chars |
| `prompt` | string | yes | Self-contained instructions for the subagent |

Spawns a subagent that inherits the parent's provider, runs isolated in its own
context, and receives only the tools admitted by the bound profile
(`EXPLORE` by default; `crates/neenee-core/src/subagent.rs`). Its final answer
is returned to the calling agent, which stays in control of all writes and any
user questions. Communication is full-duplex
([ADR-0029](../../adr/0029-full-duplex-subagent-communication.md)): a
permission or `ask_user` request the child surfaces travels up as a
`SubagentEvent`, and the user's reply travels back down via the registry +
`SubagentHandle`.

This page is the parameter reference. The subagent mechanism — isolation model,
event streaming, the TUI zoom view, profiles, and full-duplex — is explained in
[Subagents](../../explanation/agent-design/subagents.md). See also
[ADR-0011](../../adr/0011-subagent-profiles.md).
