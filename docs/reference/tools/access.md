# Tool access and capability axes

How the harness decides whether a tool may run, and whether it needs the user's
permission. This is the factual reference; the *rationale* for the tier split
and the sub-agent admission policy lives in
[Sub-agent profiles](../../explanation/agent-design/subagents.md#profiles) and
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

## `ToolAccess` tiers

`ToolAccess` (`crates/neenee-core/src/capability.rs`) is an **ordered** enum
`Read < Execute < Write`; variant order is load-bearing (it defines the `Ord`
the harness derives threshold rules from). Each consumer expresses its rule as
a threshold:

| Tier | Subagent admission | Permission broker | Examples |
|------|-----------|-------------------|----------|
| `Read` | Admitted by every profile | Bypassed | `read_file`, `grep`, `glob` |
| `Execute` | Admitted only above a `Read` ceiling | Prompted unless a cached `Always` rule matches | `bash` |
| `Write` (default) | Admitted only by a `Write` ceiling or a `write_paths` grant, then scoped by `WriteScope` | Prompted unless a cached `Always` rule matches | `write_file`, `edit_file` |

The broker prompts for any tool above `Read` (`Execute` or `Write`). Subagent
admission is by capability axis (ceiling + `write_paths` grant); a write tool
admitted to a subagent is then scoped at runtime by the agent's `WriteScope`.
A tool that does not override `access()` is treated as `Write`. All built-in
subagent profiles carry a `Read` ceiling today, so only the main agent runs
`Execute`/`Write` tools. See
[ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md).

## Capability axes

Beyond `access()`, the `Tool` trait exposes more capability bits that the
harness consults for subagent admission and control-flow gating rather than
for filesystem permissions:

| Axis | Method | Consulted by | Overrides |
|------|--------|--------------|-----------|
| Needs a human | `requires_user()` (default `false`) | Subagent profiles | `ask_user` |
| Spawns a subagent | `spawns_subagent()` (default `false`) | Subagent profiles | `subagent` |
| Affects process control | `affects_control_flow()` (default `false`) | Subagent profiles, broker bypass | `abort` |

A `requires_user()` tool is excluded from subagents unless the profile allows
user interaction; a `spawns_subagent()` tool is excluded in *every* profile to
prevent recursion. An `affects_control_flow()` tool exercises control over the
harness itself (e.g. the `abort` escape hatch) rather than the workspace — it
is **orthogonal to `access()`**, which classifies *filesystem damage*; this
axis classifies *process control*. Control tools are excluded from subagents in
*every* profile (a spawned agent must never be able to tear down the whole
program) and bypass the filesystem permission broker entirely (an escape hatch
that waits for approval is useless). See
[Subagent admission](../../explanation/agent-design/subagents.md#tool-admission).

## Permission prompt text

When the broker prompts the user, it surfaces three pieces of text from the
tool:

- The header title comes from `Tool::permission_label()`, defaulting to
  `Tool::name()`. Override when the name is a synthetic identifier that a
  user would not recognize.
- The body shown in the "Details" section comes from
  `Tool::permission_description()`, defaulting to `Tool::description()`.
  Override when `Tool::description()` is model-facing instruction prose
  (constraints aimed at the model, not a description of the call's effect).
- The `scope` line comes from `Tool::permission_scope(arguments)`.

Both `permission_label` and `permission_description` are UI-only strings.
They never reach the model and are not part of the function schema sent to
providers, so they can be reworded freely without changing tool behavior.

## See also

- [ADR-0012](../../adr/0012-toolaccess-tier-split.md) — the tier split decision.
- [Sub-agent profiles](../../explanation/agent-design/subagents.md#profiles) —
  how the axes drive subagent tool admission.
- [ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md) — the
  `WriteScope` / `write_paths` mechanism.
