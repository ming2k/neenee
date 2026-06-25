# `bash`

`BashTool` (`crates/neenee-tools/src/lib.rs`) executes a shell command. It is
the one built-in tool in the `Execute` access tier — it runs commands but is
not a file-mutation primitive, so it sits between pure reads and file writes.
The permission broker still gates it (`Execute > Read`). It is excluded from
every built-in subagent profile, all of which carry a `Read` ceiling today, so
`bash` runs only in the main agent. See [Tool access](access.md) and
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `command` | string | yes | — | Shell command line |
| `timeout` | integer | no | `30` | Seconds |

`bash` is broker-gated in the main agent: the user approves each call (or
caches an `Always` rule scoped to the command). See
[Sub-agent profiles](../../explanation/agent-design/subagents.md#profiles)
for why a command-execution role is not among the built-in profiles.
