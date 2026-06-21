# `bash`

`BashTool` (`crates/neenee-tools/src/lib.rs`) executes a shell command. It is
the one built-in tool in the `Execute` access tier — it runs commands but is
not a file-mutation primitive, so it sits between pure reads and file writes.
The permission broker still gates it (`Execute > Read`); Plan mode blocks it.
See [Tool access](access.md) and [ADR-0012](../../adr/0012-toolaccess-tier-split.md).

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `command` | string | yes | — | Shell command line |
| `timeout` | integer | no | `30` | Seconds |

The `VERIFY` sub-agent profile admits `bash` so an independent plan verifier
can run tests, builds, and type-checks as evidence; the `EXPLORE` profile does
not. See [Sub-agent profiles](../../explanation/agent-design/subagents/profiles.md).
