# Sub-agent tool admission

How a profile decides which of the parent's tools a sub-agent may actually use.
The profile primitive and the two built-in roles are covered in
[Profiles](profiles.md); this page is the per-tool decision rule and the
rationale for the exclusions.

## `ToolPolicy::admits`

Each profile carries a `ToolPolicy` (`crates/neenee-core/src/subagent.rs`) with
an `access` ceiling and an `allow_user_interaction` flag. Admission checks
three capability axes on each tool. The access axis is an ordered ceiling
(`Read < Execute < Write`); the other two are gates:

| Axis | Method | Rule |
|------|--------|------|
| Filesystem access | `Tool::access()` | Admitted when `tool.access() <= profile.access` — so `EXPLORE` drops `bash`/`Write`, `VERIFY` drops only `Write` |
| Needs a human | `Tool::requires_user()` | Excluded unless the profile opts in — `ask_user` and any future approval-gated tool |
| Spawns a sub-agent | `Tool::spawns_subagent()` | Always excluded, in *every* profile — this is what prevents recursion |

## What falls out of the policy

- **Recursion is impossible.** `task` marks itself `spawns_subagent()`, so it
  is excluded from every sub-agent regardless of profile.
  `verify_plan_execution` does the same. No name list is involved — a new
  dispatch tool that declares the axis is covered automatically.
- **The sub-agent cannot hang on the user.** `ask_user` is `Read` but
  `requires_user()`, so both built-in profiles exclude it. A sub-agent has no
  user reachable — its `UserQuestionRequest` events are dropped by the
  dispatch tool's forwarder — so admitting `ask_user` would deadlock until the
  parent turn is cancelled. Excluding it by capability is what closes that
  hole. The forwarder still has a defensive `tracing::error!` arm in case a
  future interactive tool leaks past a profile, so an invariant break is
  observable rather than turning into a silent hang.
- **The verifier can run tests, but not edit the answer.** `VERIFY`'s
  `Execute` ceiling admits `bash` (so `cargo test` / builds / type-checks count
  as evidence) but still drops `write_file`/`edit_file` — an independent
  auditor must not mutate the implementation it is auditing.
- **Goal, plan, and verify tools are inert.** They are added inside the
  sub-agent from a snapshot, tied to its own (empty) state cells — not the
  parent's. For a read-only research task they have nothing to act on.

## The snapshot

The parent toolset is snapshotted once when the dispatch tool is constructed,
after built-ins and MCP tools are assembled and before later additions (the
dispatch tool itself, the history tool). Read-only MCP servers are therefore
visible to sub-agents; tools assembled later are not. The profile then filters
that snapshot — so admission has two stages (snapshot membership, then
`admits`), and both must pass.

## Why capability axes, not a name list

An earlier design filtered with `access() == Read && name != "task"`. That was
name-driven and missed `ask_user` (which is `Read`), so the sub-agent could
call it and deadlock. The capability-axis model makes each exclusion semantic:
a tool is excluded because of *what it does* (blocks on a human, spawns an
agent, mutates the workspace), not because of what it is called. A future
interactive or dispatch tool is covered the moment it declares its axis. See
[ADR-0011](../../../adr/0011-subagent-profiles.md).

## See also

- [Profiles](profiles.md) — the roles these policies attach to.
- [Tool access](../../../reference/tools/access.md) — the `ToolAccess` tier
  reference and the `requires_user` / `spawns_subagent` axis table.
