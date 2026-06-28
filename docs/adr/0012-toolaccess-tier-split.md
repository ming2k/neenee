# 0012. ToolAccess tier split and the VERIFY profile

- **Status:** Accepted
- **Date:** 2026-06-22

## Context

ADR-0011 introduced sub-agent profiles with a tool-admission policy built on
`ToolAccess`. That left one profile (`EXPLORE`) and one ceiling (`Read`). The
second sub-agent scenario — `verify_plan_execution` — immediately exposed the
ceiling's limits.

`VerifyPlanExecutionTool` delegates to an internal `TaskTool`, so pre-0012 it
rode the `EXPLORE` profile. But the verifier's task prompt
(`crates/neenee-agent/src/plan_verify.rs`) explicitly tells the sub-agent to
use `bash for tests / builds / type-checks`, and `bash` was `Write` (the
default, no override). `EXPLORE`'s `Read` ceiling excludes every `Write` tool,
so the verifier was instructed to use a tool it could not have. It degraded to
pure static inspection — reading files instead of running them — and the prompt
was lying about the available tools. This was latent before ADR-0011 too (the
old `access() == Read` filter also excluded `bash`); the profile system just
made it visible and gave a clean handle to fix it.

The root cause was that `ToolAccess { Read, Write }` conflated two distinct
capabilities in a single binary axis:

- **command execution** (`bash` — runs commands; *can* mutate as a side effect,
  but that is not its purpose), and
- **workspace mutation** (`write_file`, `edit_file` — the tool's entire purpose
  is to edit files).

The verifier wants exactly the combination the binary cannot express: command
execution (to run `cargo test`) **without** file-write capability (an
independent auditor must not edit the implementation it is auditing). `Read`
is too tight (no bash); `Write` is too loose (admits `write_file`/`edit_file`).

Meanwhile all three consumers of `ToolAccess` — the permission broker
(`Agent::execute_tool`), the Plan-mode gate (`Tool::allowed_in_plan_mode`), and
the sub-agent profile ceiling (`ToolPolicy::admits`) — actually want *threshold*
semantics: "is this tool above or below a power line?" The binary forced each
consumer to spell that out as an equality check.

## Decision

1. **Make `ToolAccess` an ordered three-tier enum**
   (`crates/neenee-core/src/capability.rs`): `Read < Execute < Write`, with the
   derived `Ord` making variant order load-bearing. Each tier describes the
   tool's *primary capability class*:

   | Tier | Meaning | Examples |
   |------|---------|----------|
   | `Read` | Inspects state, no side effects | `read_file`, `grep`, `glob` |
   | `Execute` | Runs commands; may have external side effects, but the tool is not a file-mutation primitive | `bash` |
   | `Write` | The tool's purpose is to mutate the workspace | `write_file`, `edit_file` |

2. **Move `bash` to `Execute`** via an explicit `access()` override. It keeps
   broker-gating for the main agent (`Execute > Read`, so the broker still
   prompts) and stays blocked in Plan mode (default `allowed_in_plan_mode`
   admits `Read` only).

3. **Express every consumer as a threshold**, not an equality:

   - Permission broker: prompt when `tool.access() > Read` (i.e. `Execute` or
     `Write`). `bash` still prompts; behavior preserved.
   - Plan-mode gate: default admits `Read` only. Unchanged for every existing
     tool.
   - Profile admission: a `ToolPolicy.access` is a *ceiling*; admit when
     `tool.access() <= policy.access`. `EXPLORE` (ceiling `Read`) admits pure
     reads; `VERIFY` (ceiling `Execute`) additionally admits command execution.

4. **Add a `VERIFY` profile** (`crates/neenee-core/src/subagent.rs`) with
   ceiling `Execute`, `allow_user_interaction: false`, and a verifier-flavoured
   system prompt. It admits read tools **plus `bash`** for tests/builds/type
   checks, and still excludes `Write` tools, `requires_user` tools, and
   `spawns_subagent` tools.

5. **Bind profiles explicitly.** `TaskTool::new` now takes a
   `&'static SubagentProfile`. `task` binds `&EXPLORE`;
   `VerifyPlanExecutionTool`'s internal `TaskTool` binds `&VERIFY`. The
   verifier's user prompt is trimmed — the role framing ("independent,
   unbiased, may run commands, must not edit, non-interactive") moves to the
   `VERIFY` system prompt; the user prompt keeps only the task-specific
   contract (plan path, PASS/PARTIAL/FAIL procedure, verdict line).

## Alternatives considered

- **Named allow/deny list in `ToolPolicy`.** A `VERIFY` profile with
  `access: Read` plus an explicit `allow: ["bash"]`. Rejected: it reintroduces
  the name-driven policy that ADR-0011 replaced, and a new command-execution
  tool would need to be added to the list by hand rather than being recognised
  by its capability.

- **Keep the verifier on `EXPLORE` and delete `bash` from its prompt.** Cheapest
  by far, but it concedes that verification is static-only — the verifier can
  never run `cargo test` as evidence. That removes the most valuable signal a
  verifier can produce. Rejected.

- **Give the verifier a `Write`-ceiling profile and rely on its system prompt
  to forbid file edits.** Policy-by-prompt is exactly the failure mode the
  profile system exists to replace: a misbehaving model could edit the
  implementation it is auditing. Rejected.

- **A bitmask of independent capability flags** (`can_read`, `can_execute`,
  `can_write`) instead of an ordered enum. Rejected as over-engineering: every
  tool today occupies exactly one tier, so an ordered enum captures the reality
  with less ceremony and composes with all three consumers as a threshold.

## Consequences

Positive:

- **The verifier can finally run tests/builds/type-checks.** `VERIFY`'s
  `Execute` ceiling admits the real `bash`, so the task prompt's "bash for
  tests / builds / type-checks" is no longer aspirational.
- **Clean role/task separation.** The verifier's role contract lives in the
  `VERIFY` profile; the per-call task (plan path, report format) lives in the
  user prompt. `VerifyPlanExecutionTool` no longer reads like a role
  declaration.
- **Threshold semantics everywhere.** The broker, the Plan gate, and profile
  admission all read "above or below a line" instead of an equality, which is
  what they always meant.
- **Main-agent behaviour is preserved.** `bash` still triggers the permission
  broker (`Execute > Read`) and is still blocked in Plan mode. No user-visible
  change outside sub-agents.

Negative / honest caveat:

- `Execute`-tier tools (today just `bash`) are still *technically* capable of
  filesystem mutation via redirection (`echo > file`) or destructive commands.
  The tier describes the tool's primary purpose, not an absolute guarantee.
  Mitigation: the main agent's `bash` is still broker-gated, so the user
  approves each call; the `VERIFY` sub-agent runs with a narrow mandate and a
  system prompt that forbids edits. This is the same trust model the main agent
  already operates under — `bash` there is also not command-restricted.
- The `Execute` tier is introduced partly for one tool today. Mitigation: the
  ordering composes generally, and MCP command-execution tools can adopt the
  tier when they appear.

Migration:

- None. The change is additive for every tool except `bash`, which moves from
  `Write` to `Execute`. The only behavioural effect of that move is that the
  `VERIFY` profile now admits `bash`; broker and Plan-gate behaviour for `bash`
  is unchanged (still gated, still Plan-blocked).

## References

- `crates/neenee-core/src/capability.rs` — `ToolAccess` ordered enum + tier
  doc.
- `crates/neenee-tools/src/lib.rs` — `BashTool::access() = Execute`.
- `crates/neenee-agent/src/agent.rs::execute_tool` — broker threshold
  (`tool.access() > ToolAccess::Read`); `round_was_productive` comment.
- `crates/neenee-core/src/subagent.rs` — `ToolPolicy::admits` ceiling rule;
  `VERIFY` profile.
- `crates/neenee-agent/src/task_tool.rs` — `TaskTool::new` takes an explicit
  profile.
- `crates/neenee-agent/src/plan_verify.rs` — binds `VERIFY`; trimmed user
  prompt.
- Predecessor: [ADR-0011](0011-subagent-profiles.md) — the profile primitive
  this extends.
- [Sub-agents → Tool admission](../explanation/agent-design/envoys.md#tool-admission)
  and [Built-in tools → Tool access](../reference/tools/access.md).
