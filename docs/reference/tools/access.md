# Tool access and capability axes

How the harness decides whether a tool may run, and whether it needs the user's
permission. This is the factual reference; the *rationale* for the tier split
and the sub-agent admission policy lives in
[Sub-agent profiles](../../explanation/agent-design/subagents/profiles.md) and
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

## `ToolAccess` tiers

`ToolAccess` (`crates/neenee-core/src/capability.rs`) is an **ordered** enum
`Read < Execute < Write`; variant order is load-bearing (it defines the `Ord`
the harness derives threshold rules from). Each consumer expresses its rule as
a threshold:

| Tier | Plan mode | Permission broker | Examples |
|------|-----------|-------------------|----------|
| `Read` | Allowed | Bypassed | `read_file`, `grep`, `glob` |
| `Execute` | Blocked (above the `Read` line) | Prompted unless a cached `Always` rule matches | `bash` |
| `Write` (default) | Blocked unless `allowed_in_plan_mode` exempts the call | Prompted unless a cached `Always` rule matches | `write_file`, `edit_file` |

The broker prompts for any tool above `Read` (`Execute` or `Write`); the
Plan-mode gate admits `Read` only by default. A tool that does not override
`access()` is treated as `Write`. `write_file` and `edit_file` override
`allowed_in_plan_mode` to also permit writes under `.neenee/plans/`. See
[Plan mode](../../explanation/agent-design/plan-mode.md) for the exemption
rationale.

## Capability axes

Beyond `access()`, the `Tool` trait exposes two more capability bits that the
harness consults for sub-agent admission rather than for permissions:

| Axis | Method | Consulted by | Overrides |
|------|--------|--------------|-----------|
| Needs a human | `requires_user()` (default `false`) | Sub-agent profiles | `ask_user` |
| Spawns a sub-agent | `spawns_subagent()` (default `false`) | Sub-agent profiles | `task`, `verify_plan_execution` |

A `requires_user()` tool is excluded from sub-agents because a sub-agent has no
user reachable to answer it; a `spawns_subagent()` tool is excluded in *every*
profile to prevent recursion. `Tool::allowed_in_plan_mode` is the pre-existing
fourth axis, consulted by the Plan-mode gate. See
[Sub-agent admission](../../explanation/agent-design/subagents/admission.md).

## Permission prompt text

When the broker prompts the user, it surfaces three pieces of text from the
tool:

- The header title comes from `Tool::permission_label()`, defaulting to
  `Tool::name()`. Override when the name is a synthetic identifier that a
  user would not recognize (e.g. `create_goal` renders as `Create goal`).
- The body shown in the "Details" section comes from
  `Tool::permission_description()`, defaulting to `Tool::description()`.
  Override when `Tool::description()` is model-facing instruction prose
  (constraints aimed at the model, not a description of the call's effect).
- The `scope` line comes from `Tool::permission_scope(arguments)`.

Both `permission_label` and `permission_description` are UI-only strings.
They never reach the model and are not part of the function schema sent to
providers, so they can be reworded freely without changing tool behavior.
Only `create_goal` and `update_goal` currently override them.

## See also

- [ADR-0012](../../adr/0012-toolaccess-tier-split.md) — the tier split decision.
- [Sub-agent profiles](../../explanation/agent-design/subagents/profiles.md) —
  how the axes drive sub-agent tool admission.
- [Plan mode](../../explanation/agent-design/plan-mode.md) — the Plan-mode gate.
