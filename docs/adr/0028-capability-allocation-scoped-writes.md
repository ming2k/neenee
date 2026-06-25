# 0028. Capability allocation: scoped filesystem writes (`WriteScope`)

- **Status:** Accepted
- **Date:** 2026-06-25

## Context

[`ToolAccess`](../reference/tools/access.md) (`Read < Execute < Write`)
is the single filesystem knob today. It tiers *admission*: a sub-agent profile
sets a ceiling and admits every tool at or below it. That cannot express the
capability shape plan-as-a-subagent (ADR-0027) needs: **read tools + writes to
`.neenee/plans/` only, and no `bash`.** Raising the ceiling to `Write` to grant
the write tools also admits `Execute` (`bash`) — wrong for a planner. Keeping
the ceiling at `Read` excludes the write tools entirely.

The only path-scoped concept in the codebase today is
`Tool::allowed_in_plan_mode` + `is_plan_path` (`plan.rs`): the main agent's
Plan-mode write exemption for `.neenee/plans/`. It is bolted to Plan mode, not
a general assignable grant, so a sub-agent cannot use it.

The need is a **filesystem-write permission that is assignable to any agent**,
orthogonal to the read/execute ceiling, so a capability bundle becomes
{ceiling, write-scope} and the `PLAN` profile is just `Read` ceiling +
`Scoped(.neenee/plans)`.

## Decision

Add a runtime **`WriteScope`** capability and a declarative **`write_paths`**
grant on `ToolPolicy`. Admission of write tools is decoupled from the ceiling.

### `WriteScope` (runtime, per agent)

```rust
// neenee-core/src/capability.rs
pub enum WriteScope {
    None,                       // read-only / execute-only agents
    Scoped(Vec<PathBuf>),       // writes only under these canonical dirs
    Unrestricted,               // the main agent
}
```

It is a **hard boundary, not a prompt**: writes outside the scope are blocked
outright. Orthogonal to `ToolAccess` — that admits *whether* a tool runs;
`WriteScope` scopes *where* an admitted write tool may land.

### `write_paths` (declarative, on `ToolPolicy`)

```rust
// neenee-core/src/subagent.rs
pub struct ToolPolicy {
    pub access: ToolAccess,
    pub allow_user_interaction: bool,
    /// Relative dir specs a sub-agent may write to, beyond what `access`
    /// admits. Empty = no grant. Resolved to a runtime WriteScope against
    /// the project cwd at spawn time.
    pub write_paths: &'static [&'static str],
}
```

### Admission — write decoupled from the ceiling

```text
admits(tool):
  spawns_subagent                       → deny (absolute)
  requires_user && !allow_user_interaction → deny
  access <= ceiling                     → admit              (Read/Execute)
  access == Write && write_paths != []  → admit              (scoped grant)
  else                                  → deny
```

So `Read` ceiling + non-empty `write_paths` admits read tools **and** write
tools (scoped), but **not** `bash` (`Execute` is never granted via
`write_paths`). That is exactly the `PLAN` shape. Every existing profile sets
`write_paths: &[]`, so its admission is unchanged.

### Enforcement — one gate, before the broker

`Agent::execute_tool` gains a write-scope check in its gating stack, right
after the Plan-mode gate and before the permission broker:

- the main agent carries `WriteScope::Unrestricted` → the gate is a no-op and
  the broker still prompts (once/always/reject) — unchanged behavior;
- a sub-agent carries the `WriteScope` resolved from its profile — a write
  tool whose target (from `tool.permission_scope`, which write tools already
  override to the path) is not under the scope is blocked with a tool result,
  not an error.

Path resolution mirrors the existing `is_plan_path` logic: canonicalize the
parent and re-append the file name so a not-yet-existing file still resolves.

### Per-profile allocation

| Agent | ceiling | write_paths | Effective tools |
|---|---|---|---|
| Main | `Write` | — (Unrestricted) | all; writes still broker-gated |
| `EXPLORE` | `Read` | `&[]` | read-only |
| `VERIFY` | `Execute` | `&[]` | read + `bash`, no writes |
| `REVIEW` | `Read` | `&[]` | read-only |
| `TITLE` | `Read` | `&[]` | read-only |
| `PLAN` (ADR-0027) | `Read` | `[".neenee/plans"]` | read + writes scoped to plans, no `bash` |

## Alternatives considered

- **Raise the `PLAN` ceiling to `Write`.** Rejected: ordered `ToolAccess`
  makes `Write` imply `Execute`, so the planner would get `bash`. The whole
  point is a planner that can persist a plan but not run commands.

- **Explicit per-profile tool allow-lists (name the tools each profile gets).**
  Rejected for now: the capability axis already selects tools cleanly for
  read/execute, and `write_paths` adds the missing filesystem dimension
  without a name registry that drifts as tools are added. Allow-lists remain a
  future option if a profile ever needs a non-capability-shaped set.

- **Put the path exemption in admission rather than at runtime enforcement.**
  Rejected: admission is `const`/static (`&'static [&'static str]`); the
  actual canonical paths depend on the project cwd at spawn time, so the
  check must run at the execution funnel. The split — declarative grant on the
  profile, runtime `WriteScope` resolved and enforced on the agent — matches
  how `access` (static) and the broker (runtime) already split.

- **Make `WriteScope` a prompt like the broker.** Rejected: a sub-agent has no
  user reachable to answer a prompt, and the boundary is a capability limit,
  not a confirmation. The broker stays the main agent's interactive layer
  *inside* an `Unrestricted` scope.

## Consequences

- **Positive:** one assignable filesystem-permission primitive enables the
  `PLAN` profile (ADR-0027) and any future scoped-write role. When ADR-0027
  lands, `allowed_in_plan_mode` + `is_plan_path` fold into `WriteScope` and
  the special case disappears. The change is additive: every built-in profile
  keeps `write_paths: &[]`, so admission and the main agent are unchanged.

- **Negative:** `ToolPolicy` gains a field, so every literal (four profiles +
  tests) is touched — mechanical. Enforcement adds one path-canonicalization
  per write call, negligible.

- **Neutral:** `ToolAccess` and its ordering are untouched; the broker is
  untouched. ADR-0011/0012's capability axis is extended, not replaced.

- **Migration (each step shippable):**
  1. `WriteScope` + `ToolPolicy::write_paths` + decoupled admission + profile
     defaults `&[]` (zero behavior change). — this change
  2. `Agent` carries `WriteScope` (default `Unrestricted`); `execute_tool`
     enforces it for write tools; `TaskTool` resolves the profile's scope
     against cwd and sets it on the sub-agent. — this change
  3. (ADR-0027) introduce the `PLAN` profile using `write_paths:
     [".neenee/plans"]`; later remove `allowed_in_plan_mode` / `is_plan_path`
     once Plan mode is gone.

## References

- [ADR-0011](0011-subagent-profiles.md) — the capability-axis profile
  primitive this extends.
- [ADR-0012](0012-toolaccess-tier-split.md) — the `ToolAccess` tiers whose
  write tier is decoupled by `write_paths`.
- [ADR-0026](0026-plan-progression-forcing-functions.md) — unaffected.
- [ADR-0027](0027-plan-as-subagent.md) — the `PLAN` profile this enables.
- [ADR-0033](0033-remove-plan-and-verify-workflow.md) — the workflow that
  consumed it was later removed.
