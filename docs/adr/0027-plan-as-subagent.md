# 0027. Plan as a subagent (replace Plan mode with a `PLAN` profile + a `plan` tool)

- **Status:** Superseded by ADR-0033
- **Date:** 2026-06-25

> Superseded by [ADR-0033](0033-remove-plan-and-verify-workflow.md). The
> `PLAN` profile, the `plan` tool, and `verify_plan_execution` were removed.

> This supersedes [ADR-0006](0006-plan-mode-v2.md) (plan mode v2) and revises
> [ADR-0026](0026-plan-progression-forcing-functions.md) (the plan-exit nudge
> is removed — there is no Plan mode to stall). It depends on
> [ADR-0028](0028-capability-allocation-scoped-writes.md) (`WriteScope`) and,
> for inline clarification, [ADR-0029](0029-full-duplex-subagent-communication.md)
> (full-duplex).

## Context

Plan is currently a **mode** — a second value of an `AgentMode { Build, Plan }`
enum on the main agent. Entering Plan mode flips a per-tool
`allowed_in_plan_mode` gate that collapses the tool surface to read-only plus
writes under `.neenee/plans/`; `plan_enter` / `plan_exit` flip the mode
in place, and `plan_exit` raises the approval gate. This is ADR-0006.

Two things make that shape awkward:

1. **A parallel gating mechanism.** neenee already has a tool-admission
   primitive — the subagent `SubagentProfile` capability axis
   (`Tool::access` / `requires_user` / `spawns_subagent`, ADR-0011/0012).
   Plan mode is a *second* mechanism keyed off the same `ToolAccess` tier:
   `allowed_in_plan_mode` defaults to `access == Read` and adds one path
   exemption. Two gates, one foundation, duplicated reasoning. Every tool
   that is read-only declares it once for the profile axis and once for the
   mode gate.

2. **Planning bloats the main transcript.** In Plan mode the *main* agent does
   the research, so every read-only exploration round lands in the main
   conversation — the very context ADR-0019/0023 work to keep small. A plan
   that takes fifteen read rounds to research eats the budget the implementer
   needs. And because it is one agent, research cannot run in parallel.

The forcing functions added in [ADR-0026](0026-plan-progression-forcing-functions.md)
are themselves mode-shaped — the plan-exit nudge exists only because a Plan
mode can stall on plain text before approval.

The reference points agree on a different shape. **opencode** models plan as a
distinct `plan` *agent* (read-only), separate from the `build` agent.
**Claude Code** keeps a plan *mode* as the conversational shell, but its
5-phase workflow delegates the actual research and design to `explore` and
`plan` **subagents** spawned inside it. In both, the heavy, parallelisable,
context-isolating work is a subagent, not a mode of the main loop.

## Decision

Reframe plan as a **subagent**. Remove the mode. Concretely:

### 1. Drop `AgentMode`; the main agent is single-mode

The `AgentMode` enum, the shared mode cell, the `ModeChanged` event, the
`/mode` command, and the TUI mode indicator are removed. The main agent
always has its full tool surface. There is no Plan mode to gate, so
`Tool::allowed_in_plan_mode` and the per-tool `is_plan_path` exemption on
`write_file` / `edit_file` are removed.

### 2. Add a `PLAN` subagent profile (read-only, non-interactive)

A new built-in profile alongside `EXPLORE` / `VERIFY` / `REVIEW` / `TITLE`,
admitted by the existing capability axis — no new mechanism:

```rust
pub const PLAN: SubagentProfile = SubagentProfile {
    name: "plan",
    system_prompt: /* research the request, design the change,
                       return the plan as markdown — non-interactive */,
    tool_policy: ToolPolicy {
        access: ToolAccess::Read,
        allow_user_interaction: false,
    },
};
```

Read-only, non-interactive, non-recursive — like `EXPLORE` in capability,
framed for producing a plan rather than free-form findings.

### 3. The `PLAN` subagent writes its own plan, scoped via `WriteScope`

[ADR-0028](0028-capability-allocation-scoped-writes.md) adds an assignable,
per-agent filesystem-write boundary (`WriteScope`) plus a `write_paths` grant
on `ToolPolicy` that decouples write admission from the `ToolAccess` ceiling.
The `PLAN` profile uses exactly that:

```rust
pub const PLAN: SubagentProfile = SubagentProfile {
    name: "plan",
    system_prompt: /* research the request, design the change,
                       write the plan to .neenee/plans/<slug>.md */,
    tool_policy: ToolPolicy {
        access: ToolAccess::Read,
        allow_user_interaction: false,
        write_paths: &[".neenee/plans"],
    },
};
```

So the planner gets read tools **plus** write tools scoped to
`.neenee/plans/` (enforced at the `execute_tool` funnel), and **no `bash`**
(`Execute` is never granted via `write_paths`). It writes the plan file
itself; its result to the main agent is just a completion signal and the
plan path — the main agent reads the path for approval (§4). Admission stays
pure capability-axis; there is no path-exemption special case on the write
tools.

### 4. Replace `plan_enter` / `plan_exit` with one `plan` tool

A single tool the main agent calls to delegate planning:

1. spawn the `PLAN` subagent with the user's request (+ any clarifications
   already gathered);
2. on its return, read the plan file the subagent wrote, then raise the
   **approval gate** (the same *Approve* / *Keep planning* prompt ADR-0006
   introduced);
3. on *Approve* — set `active_plan_path`, seed the todo list from the plan's
   `##` headings, and return the
   [ADR-0026](0026-plan-progression-forcing-functions.md) approval-handoff
   instruction ("start coding now, track with `todo` / `todo_update`, do not
   end the turn until done");
4. on *Keep planning* — return the user's feedback to the main agent, which
   re-calls `plan` with the refined request.

The `plan` tool declares `spawns_subagent: true`, so the existing recursion
guard excludes it from every subagent automatically — no extra rule needed.

### 5. Clarification is inline, via full-duplex (ADR-0029)

The first draft of this ADR kept `PLAN` non-interactive and routed
clarification through the parent as a coarse round-trip (child returns "I need
X", parent asks, re-spawns). That was a workaround for subagents being
fire-and-forget. [ADR-0029](0029-full-duplex-subagent-communication.md) adds a
live channel — the child surfaces `ask_user`/permission *up* as a
`SubagentEvent`, and the user's reply travels *down* via the registry →
`SubagentHandle` → the child's parked oneshot. So `PLAN` can clarify inline
like the main agent does, with no re-spawn. (While ADR-0029's
`set_unattended` transitional gate is still in place, the default `PLAN`
profile remains non-interactive in practice; an interactive `PLAN` is one
profile flag away once the gate is dropped.)

### 6. `active_plan_path` and verification are unchanged

`active_plan_path` (set by the `plan` tool on approval) still drives the
Build-mode system-prompt hint and `verify_plan_execution`. The
verify gate + two-phase pipeline is orthogonal to how the plan was produced
and needs no change (both were later removed by
[ADR-0033](0033-remove-plan-and-verify-workflow.md)).

### 7. Forcing functions (ADR-0026) re-anchor

- **plan-exit nudge — removed.** No Plan mode can stall on plain text; the
  `plan` tool either returns an approval decision or feedback to re-plan.
- **approval-handoff + todo-continuation nudges — survive**, unchanged in
  behavior. They now trigger off the `plan` tool's approved result and the
  seeded todo list rather than a mode flip.
- **verify-nudge — unchanged.**

## Alternatives considered

- **Keep Plan as a mode (status quo + ADR-0026).** Rejected: it leaves the
  duplicated gating mechanism and the main-transcript bloat, and the
  plan-exit nudge exists only to paper over a mode-shaped stall. The
  subagent reframe removes all three at once.

- **Hybrid: keep a thin Plan mode as the approval/clarification shell, but
  delegate research/design to subagents (the claude/opencode actual shape).**
  Considered most seriously. It captures isolation and parallelism without
  losing inline `ask_user` clarification. Rejected because it preserves the
  very `AgentMode` / `allowed_in_plan_mode` duplication the reframe exists to
  remove — the shell is the part that does not pull its weight. Inline
  clarification is nice but not worth a second gating mechanism; the
  parent-routed round-trip (§5) covers it with zero new plumbing.

- **A bespoke path-scoped write exemption on `write_file` / `edit_file`.**
  Rejected: that re-introduces the `is_plan_path` special case under a new
  name. Instead the `PLAN` profile's scoped write is expressed through the
  general `WriteScope` / `write_paths` mechanism of [ADR-0028](0028-capability-allocation-scoped-writes.md),
  which any future profile can reuse — one assignable permission primitive,
  not a plan-only exemption. The earlier draft of this ADR had the `PLAN`
  subagent stay read-only and return the plan as content for the main agent
  to persist; that avoided the exemption but left the planner unable to write
  its own drafts and was reversed once ADR-0028 made scoped writes general.

- **Make `PLAN` an interactive profile (`allow_user_interaction: true`) with
  subagent→user passthrough.** Rejected: neenee subagents are fire-and-forget
  by design (`UserQuestionRequest` from a subagent is dropped today; the
  plumbing to surface and route it does not exist). Building passthrough is
  new mechanism, not reuse — the opposite of the motivation. The
  parent-routed round-trip achieves the same outcome without it.

- **Two tools (`plan_start` / `plan_finish`) mirroring today's
  `plan_enter` / `plan_exit`.** Rejected: with the mode gone there is no
  state for a "start" to set. One `plan` tool that spawns, approves, and
  persists in a single call is simpler and maps cleanly to "delegate, then
  decide on the result."

## Consequences

- **Positive:** one tool-admission mechanism (the capability axis) replaces
  two; planning research runs in an isolated context and no longer bloats the
  main transcript; multiple `PLAN` subagents can explore approaches in
  parallel (future); the main loop loses its Plan-mode branch, and the
  plan-exit nudge (ADR-0026) dissolves. The change reuses the existing
  `SubagentProfile` / `TaskTool` machinery and the existing subagent TUI
  rendering — no new control-plane concept.

- **Negative:** clarification is a coarser parent round-trip, not inline — a
  plan that needs several Q&A exchanges pays a full subagent spawn per
  exchange. The approval flow, the `ModeChanged` event, the `/mode` command,
  and the TUI header indicator are rewritten. The persisted session `mode`
  field is removed and old sessions migrated (a `mode: Plan` snapshot resumes
  as default, since the main agent is now always single-mode). ADR-0006 is
  superseded; ADR-0026 is revised (plan-exit nudge removed).

- **Neutral:** ADR-0009 (uncapped loop) is untouched — the main loop was
  already single-mode in practice during Build. `/pursue`, `/repeat`, skills,
  and MCP are untouched. The `task` / `EXPLORE` path is untouched; `PLAN` is
  a sibling profile.

- **Migration (phased, each step shippable):**
  1. Add the `PLAN` profile (`neenee-core/src/subagent.rs`) — pure addition.
  2. Add the `plan` tool in `neenee-agent`, internally spawning the `PLAN`
     subagent via `TaskTool` machinery and reusing the existing approval
     prompt from `execute_plan_exit`. Coexist with the old mode tools.
  3. Switch docs/prompt to direct the model to `plan` instead of
     `plan_enter` / `plan_exit`.
  4. Remove `AgentMode`, `plan_enter`, `plan_exit`, `allowed_in_plan_mode`,
     the `is_plan_path` write exemption, the `ModeChanged` event, the `/mode`
     command, and the TUI mode indicator; migrate the persisted `mode` field.
  5. Revise ADR-0026 (drop the plan-exit nudge), supersede ADR-0006, flip
     this ADR to Accepted, update `plan.md`.

## References

- [ADR-0006](0006-plan-mode-v2.md) — the mode-based design this supersedes
  on acceptance.
- [ADR-0009](0009-uncapped-agentic-loop.md) — the uncapped loop; untouched.
- [ADR-0011](0011-subagent-profiles.md) — the capability-axis profile
  primitive the `PLAN` profile reuses.
- [ADR-0012](0012-toolaccess-tier-split.md) — the `ToolAccess` tiers the
  profile admits on.
- [ADR-0026](0026-plan-progression-forcing-functions.md) — revised: the
  plan-exit nudge is removed; the approval-handoff and todo-continuation
  nudges survive, re-anchored on the `plan` tool result.
- [ADR-0028](0028-capability-allocation-scoped-writes.md) — the `WriteScope` /
  `write_paths` mechanism the `PLAN` profile's scoped write uses; a prerequisite.
- [ADR-0033](0033-remove-plan-and-verify-workflow.md) — this workflow was
  later removed.
- opencode — plan as a distinct read-only `plan` agent separate from `build`.
- Claude Code `2.1.190` — research/design delegated to `explore` / `plan`
  subagents inside the plan workflow.
