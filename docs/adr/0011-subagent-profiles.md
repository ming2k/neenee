# 0011. Sub-agent profiles: capability-axis tool admission

- **Status:** Accepted
- **Date:** 2026-06-22

## Context

The `task` tool spawns an autonomous, non-interactive sub-agent for research
subtasks. Until this decision, the sub-agent's toolset was assembled by a
hardcoded filter inside `TaskTool`:

```rust
// crates/neenee-agent/src/task_tool.rs (pre-0011)
.filter(|tool| tool.access() == ToolAccess::Read && tool.name() != "task")
```

The same filter was duplicated a second time in `Agent::new`
(`crates/neenee-agent/src/agent.rs`) to build the toolset for the
`VerifyPlanExecutionTool`'s internal verifier sub-agent. Two problems followed
from the filter's shape.

**1. `ask_user` reached the sub-agent and deadlocked.** `AskUserTool` returns
`ToolAccess::Read` (it does not mutate the workspace), so it passed the filter.
But `ask_user` is handled specially in `Agent::execute_tool`
(`agent.rs::execute_ask_user`): it inserts a pending oneshot, emits an
`AgentEvent::UserQuestionRequest`, and `receiver.await`s the user's reply. A
sub-agent is a separate `Agent` whose events all flow through
`TaskTool::forward_event`, which forwards only five event shapes and drops the
rest via `_ => {}`. `UserQuestionRequest` is not among the forwarded shapes, so
the request never reached any user. The sub-agent then blocked on
`receiver.await` until the parent turn was cancelled and the future dropped —
at which point the receiver resolved to `None` and the sub-agent returned
"User cancelled the question" as its answer, having burned the tokens. The
user could *see* the `ask_user` call in the nested TUI view (the `ToolCall`
event is forwarded) but could not answer it.

**2. The policy was name-driven and buried in orchestration.** Recursion was
prevented by the literal `tool.name() != "task"`, not by any semantic property
of the tool. Adding a second dispatch tool, or a second sub-agent role with a
different tool policy, meant editing the dispatch tool rather than declaring
intent. The capability axis neenee already had — `Tool::access` (Read/Write) —
described *filesystem mutation*, which is the wrong axis for "may block on a
human": `ask_user` is `Read` but fundamentally interactive. Plan mode had
already established the pattern of per-tool capability bits via
`Tool::allowed_in_plan_mode`; sub-agent admission was the odd one out.

A review of the two reference implementations (codex `forkSubagent`,
claude-code `forkSubagent.ts`) confirmed the consensus shape: a sub-agent is
constructed with an explicit *subset* of tools, and the dispatch tool is the
natural place to express that subset — but expressing it as a declarative
policy (not an inline `filter` closure) is what keeps it honest as tools
accrue.

## Decision

1. **Add two capability axes to the `Tool` trait**
   (`crates/neenee-core/src/capability.rs`), parallel to the existing
   `allowed_in_plan_mode`:

   - `requires_user() -> bool` (default `false`) — the tool's execution may
     block awaiting a live human decision. `AskUserTool` overrides to `true`.
   - `spawns_subagent() -> bool` (default `false`) — invoking this tool
     dispatches a nested agent. `TaskTool` and `VerifyPlanExecutionTool`
     override to `true`.

2. **Introduce a `SubagentProfile` primitive in `neenee-core`**
   (`crates/neenee-core/src/subagent.rs`), peers with `AgentMode`. A profile
   carries a `name`, a `system_prompt` fragment, and a `ToolPolicy`. The
   policy has two fields:

   - `access: ToolAccess` — the maximum filesystem access a sub-agent under
     this profile may have (a ceiling: `Read` admits only `Read` tools).
   - `allow_user_interaction: bool` — whether `requires_user()` tools may run.

   `ToolPolicy::admits(&dyn Tool) -> bool` is the single admission predicate.
   A tool that `spawns_subagent()` is **always** excluded (recursion is
   absolute, not a per-profile toggle); `access` and `requires_user` are then
   checked against the policy.

3. **Ship one built-in profile, `EXPLORE`**, and bind `TaskTool` to it
   (`&EXPLORE`). Its policy is read-only and non-interactive. Its system
   prompt is the single source of the sub-agent's framing text, replacing the
   inline `format!` that previously lived in `TaskTool::run_sub_agent_outcome`.

4. **Drive admission from the profile.** `TaskTool` replaces its inline
   `filter` with `self.profile.select_tools(&self.tools)`. `Agent::new` drops
   its duplicated `Read && name != "task"` filter for the verifier sub-agent
   and hands the raw input toolset to `VerifyPlanExecutionTool`; the internal
   `TaskTool` applies the profile itself. The admission rule now lives in
   exactly one place.

5. **Defense in depth.** `TaskTool::forward_event` adds an explicit
   `AgentEvent::UserQuestionRequest` arm that emits `tracing::error!` and
   drops the request. The profile is meant to keep `requires_user` tools out
   of the sub-agent entirely; if one ever leaks through, the invariant break
   is observable in logs rather than silently turning into the pre-0011 hang.

## Alternatives considered

- **Name-based blacklist (the one-line fix).** Add `ask_user` to the existing
  `!matches!(tool.name(), "task" | "ask_user")` filter. Rejected: it leaves
  the policy name-driven and buried, so the next interactive tool (or the next
  dispatch tool) re-introduces the same class of bug. It also leaves the
  duplicated filter in `Agent::new` intact.

- **Make `requires_user` an absolute exclude (no policy knob).** Treat
  `requires_user()` tools like `spawns_subagent()` — always excluded from any
  sub-agent, with no `allow_user_interaction` field. Rejected for this ADR:
  it forecloses a future interactive sub-agent role (one where
  `UserQuestionRequest` is genuinely forwarded to the user) without saving any
  real complexity. The policy knob is one `bool`; keeping it makes the profile
  a real permission surface rather than a single hard-coded shape. `EXPLORE`
  leaves it off today.

- **Externalize profiles to config files** (à la opencode's
  `.opencode/agent/<name>.md` with per-agent `permission`). Rejected for now:
  neenee has no on-disk agent-config tradition, and there is exactly one
  profile. A `const` in `neenee-core` (peers with `AgentMode`) is the right
  size. Externalization can follow under a later ADR when a second role
  actually exists.

- **Surface a role selector on `task` now.** Add a `role` parameter the model
  fills. Rejected: with one profile, the knob has only one legal value and the
  model has no basis to choose it. Keep `task`'s schema as
  `description` + `prompt`; bind the profile implicitly. Revisit when a second
  profile lands.

## Consequences

Positive:

- **The deadlock is gone by construction.** `ask_user` cannot reach a
  sub-agent, so `execute_ask_user`'s `receiver.await` can never fire inside
  one. Even a future leak is logged instead of hanging.
- **One admission rule, one place.** `ToolPolicy::admits` is the single source
  of truth; `TaskTool` and the verifier path both go through it. The
  duplicated `Read && name != "task"` filter is deleted.
- **Recursion is semantic.** A new dispatch tool that marks itself
  `spawns_subagent()` is excluded from sub-agents without anyone remembering
  to extend a name list.
- **Profiles are a real extension point.** Adding a second role (e.g. a
  write-capable executor, or a future interactive role) is a new `const
  SubagentProfile` plus a binding in `TaskTool` — no orchestration surgery.

Negative:

- Two new methods on the `Tool` trait. Every `Tool` impl already overrides
  `access` and several override `allowed_in_plan_mode`; the two new defaults
  are non-breaking and the burden is one line per interactive/dispatch tool.
  Mitigation: defaults keep existing tools unchanged.

Migration:

- None. The decision is taken as incompatible-with-history per the
  implementer's brief: no compat shim for the old `Read && name != "task"`
  filter is kept. The sub-agent toolset after this ADR is a strict subset of
  the pre-0011 one (it additionally excludes `ask_user`), so no caller gains a
  capability it should not have.

## References

- `crates/neenee-core/src/capability.rs` — `Tool::requires_user`,
  `Tool::spawns_subagent`.
- `crates/neenee-core/src/subagent.rs` — `ToolPolicy`, `SubagentProfile`,
  `EXPLORE`, and the `admits` / `select_tools` admission surface with tests.
- `crates/neenee-tools/src/lib.rs` — `AskUserTool::requires_user() = true`.
- `crates/neenee-agent/src/task_tool.rs` — `TaskTool` bound to `EXPLORE`;
  `forward_event` defensive arm for `UserQuestionRequest`.
- `crates/neenee-agent/src/plan_verify.rs` —
  `VerifyPlanExecutionTool::spawns_subagent() = true`.
- `crates/neenee-agent/src/agent.rs` — `Agent::new` verifier-toolset cleanup.
- Predecessor: [ADR-0005](0005-strict-layering-and-renames.md) — why
  `TaskTool` lives in `neenee-agent` (orchestration primitive), which is why
  the profile primitive lives one layer down in `neenee-core`.
- [Sub-agents](../explanation/agent-design/subagents.md) — rewritten tool
  admission section.
- [Built-in tools](../reference/tools/index.md) — `task` special-tool entry and the
  new capability axes.
