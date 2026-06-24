# Subagents

The `subagent` tool spawns an isolated child agent to investigate a sub-question
and return a written answer. The parent agent stays in control of writes to the
workspace (outside what a child's profile grants) and of the top-level
conversation. For the tool's parameters and access class, see
[`subagent`](../../reference/tools/subagent.md).

## Why a subagent tool

A single agent turn accumulates context: every file read, every grep result,
every tool round stays in the transcript. For a large investigation that touches
many unrelated corners of the codebase, one of two things happens — either the
context fills with material only loosely related to the final answer, or the
model spends turns re-reading things it already saw. A subagent gives the model
a way to delegate the exploration:

1. **Context isolation.** The subagent runs with a fresh two-message history
   (its task prompt plus the system prompt). Its tool rounds never enter the
   parent's transcript; only its final summary does.
2. **Capability-bounded by construction.** The subagent receives only the tools
   its profile admits, scoped by a per-agent `WriteScope`, so it can only do
   what its profile declares — never trigger the parent's permission broker,
   never exceed its granted write paths. See [Profiles](#profiles) and
   [Tool admission](#tool-admission).
3. **Parallelizable investigation.** The model can dispatch several `subagent`
   calls to map different parts of a problem, then act on the synthesized
   findings.

## The `subagent` tool

The `subagent` tool is the one built-in tool whose result is not a single value
but a streamed investigation. It takes a short description and a prompt, both
required, and returns a payload carrying the subagent's summary, its full
transcript, and its token usage. The parent persists that transcript as the
tool step's children, so `/resume` rebuilds the nested view later.

Because the subagent's progress is interesting in real time (not just its final
answer), the tool streams live rather than blocking until completion: every
token and tool round the child produces is relayed to the parent TUI as it
happens. Input validation rejects only non-JSON or empty-after-trim fields; the
length hint on the description is a model-facing nudge, not an enforced bound.

## Isolation model

The subagent shares exactly one thing with the parent — the model provider —
and nothing else:

| Concern | Shared? | How |
|---------|---------|-----|
| Provider | Yes | The same provider connection |
| Conversation history | No | A fresh system + task prompt |
| Tools | Snapshot, profile-filtered | The tools the bound [profile](#profiles) admits |
| Write boundary | No | A `WriteScope` resolved from the profile's `write_paths` grant |
| Pursuit state | No | An empty in-memory pursuit store |
| Plan state | No | No active plan (a planner child writes one; it does not inherit one) |
| Skills | No | No loaded skills |
| Cancellation token | No | A fresh, independent token |
| Session persistence | No | The subagent is never persisted |

The filesystem is implicitly shared because the subagent inherits the process
working directory; its profile gates *what* it may write through the
`WriteScope` boundary (ADR-0028).

## Profiles

A subagent's behaviour is not hardcoded in the dispatch tool. It is the output
of a declarative **profile** — a name, a system-prompt fragment that frames the
role, and a `ToolPolicy` that scopes what it may touch. Profiles are domain
vocabulary; the dispatch tools bind them by reference.

### The built-in profiles

| Profile | Bound by | Ceiling | Write grant | Gets |
|---------|----------|---------|-------------|------|
| `EXPLORE` | `subagent` | `Read` | none | Pure read tools (`read_file`, `grep`, `glob`, `list_dir`, …) |
| `VERIFY` | `verify_plan_execution` | `Execute` | none | Read tools **plus `bash`** for tests/builds/type-checks |
| `PLAN` | `plan` | `Read` | `.neenee/plans` | Read tools **plus scoped writes** to the plans dir, no `bash` |

All three are non-interactive (`allow_user_interaction: false`) today and
non-recursive (recursion is excluded absolutely, not per-profile — see
[Tool admission](#tool-admission)). The profile is the single source of truth;
the dispatch tool takes the profile explicitly, the verifier goes through the
same dispatch machinery, and the planner is a third binding.

### Why three roles

`EXPLORE` is the research role: pure inspection, no side effects. A researcher
should not run commands — an exploration subagent with `bash` could mutate the
workspace or run arbitrary commands, which is wrong for "go find things and
report back".

`VERIFY` is the independent-auditor role. Its most valuable evidence is
*behaviour*: does `cargo test` pass, does it build, does it type-check? Static
inspection alone cannot answer those, so the verifier needs command execution.
But it must still not edit the implementation it is auditing.

`PLAN` is the designer role. It researches read-only and writes the plan to
`.neenee/plans/`, but must not run commands (a planner does not execute the
change) and must not touch source. It gets write tools scoped to the plans
directory via a `write_paths` grant, without raising its access ceiling to
`Execute` (which would admit `bash`). See
[ADR-0027](../../adr/0027-plan-as-subagent.md) and
[ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md).

The three needs (no commands; commands-but-no-file-writes; writes-but-no-commands)
cannot be expressed by a single ceiling. The `Read < Execute < Write` tier split
plus the decoupled `write_paths` grant is what resolves them: `VERIFY`'s
`Execute` ceiling admits `bash` while excluding writes; `PLAN`'s `Read` ceiling
plus `write_paths` grant admits scoped writes while excluding `bash`. See
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

### Extending

Adding a fourth role is a new profile constant plus a binding at the dispatch
site — no orchestration surgery, no changes to the admission rule. An
interactive role (one whose `ask_user` requests are forwarded to the user) would
land here; the full-duplex channel ([ADR-0029](#full-duplex)) already carries
it. The profile primitive was introduced in
[ADR-0011](../../adr/0011-subagent-profiles.md), extended to two roles + the
tier split in [ADR-0012](../../adr/0012-toolaccess-tier-split.md), and to the
scoped-write `PLAN` role in
[ADR-0027](../../adr/0027-plan-as-subagent.md) / [ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md).

## Tool admission

How a profile decides which of the parent's tools a subagent may actually use.

### The admission check

Each profile carries a `ToolPolicy` with an `access` ceiling, an
`allow_user_interaction` flag, and a `write_paths` grant. Admission checks
capability axes on each tool. The access axis is an ordered ceiling
(`Read < Execute < Write`); write tools below the ceiling are additionally
admitted by a non-empty `write_paths` grant; the other two axes are gates:

| Axis | Rule |
|------|------|
| Filesystem access | Admitted when the tool's tier is at or below the ceiling — so `EXPLORE` drops `bash`/`Write`, `VERIFY` drops only `Write` |
| Scoped write | A `Write` tool below the ceiling is admitted when `write_paths` is non-empty (then scoped at runtime) — this is how `PLAN` gets writes-without-`bash`. `Execute` is never granted this way |
| Needs a human | Excluded unless the profile opts in — `ask_user` and any future approval-gated tool |
| Spawns a subagent | Always excluded, in *every* profile — this is what prevents recursion |

### What falls out of the policy

- **Recursion is impossible.** `subagent` marks itself `spawns_subagent()`, so
  it is excluded from every subagent regardless of profile. `plan` and
  `verify_plan_execution` do the same. No name list is involved — a new
  dispatch tool that declares the axis is covered automatically.
- **Scoped writes, not blanket writes.** A `Write` tool admitted by `write_paths`
  is then scoped at runtime by the agent's `WriteScope`: the `PLAN` profile's
  writes resolve to `.neenee/plans/` and a write anywhere else is blocked at
  the execution funnel. Admission says *whether*; `WriteScope` says *where*.
- **Pursuit, plan, and verify tools are inert.** They are added inside the
  subagent from a snapshot, tied to its own (empty) state cells — not the
  parent's. For a read-only research task they have nothing to act on.

### Why capability axes, not a name list

An earlier design filtered by access tier plus a name exclusion. That was
name-driven and missed `ask_user` (which is `Read`), so the subagent could call
it and deadlock. The capability-axis model makes each exclusion semantic: a
tool is excluded because of *what it does* (blocks on a human, spawns an agent,
mutates the workspace), not because of what it is called. A future interactive
or dispatch tool is covered the moment it declares its axis. See
[ADR-0011](../../adr/0011-subagent-profiles.md).

## Full-duplex

A subagent is **not** fire-and-forget. Communication is full-duplex
([ADR-0029](../../adr/0029-full-duplex-subagent-communication.md)): a request
the child surfaces travels *up* to the parent, and a reply travels *down* into
the exact child that surfaced it, while the child runs.

- **Up.** The child's `PermissionRequest` and `UserQuestionRequest` events are
  forwarded as `SubagentEvent`s to the parent harness (and through it to the
  TUI), instead of being dropped. The child's own broker / `ask_user` oneshot
  stays parked until a reply arrives.
- **Down.** Each child installs a steering inbox and exposes a `SubagentHandle`
  (`Weak<Agent>` + a sender). The dispatch tool keeps a `SubagentRegistry`
  mapping the parent tool-call id to the live handle. A reply arriving on a
  nested request is routed `registry → handle → reply_permission` /
  `reply_user_question`, resolving the child's parked oneshot directly.

Two channels, deliberately split: **steering** (`submit` an `AgentOp`) queues
onto an inbox drained at the next tool-round boundary, so it can never
interrupt a side effect mid-flight; **request/reply** resolves a parked oneshot
immediately, since the child is already waiting on it.

The built-in profiles stay non-interactive, so in practice no nested request is
surfaced today and the child's `set_auto_approve(true)` is a transitional gate
rather than a load-bearing deadlock fix. An interactive profile that opts into
`allow_user_interaction` can drop that gate and the round-trip just works.

## Runtime

How a subagent executes once dispatched: the event stream it emits, how the TUI
renders it, and how it fails.

### Event streaming

The subagent is a real agent, so it emits the full event stream a parent does.
Each child event is wrapped and forwarded to the parent, so the TUI builds the
nested view in real time:

```text
subagent event ──forward──► wrapped child event
                                │
parent dispatch        ──► parent event carrying the child event
                                │
orchestration relay    ──► response the TUI appends to the matching step
```

The forwarded events carry the same shapes the parent stream does — streaming
deltas, tool calls, tool results, activity, and (full-duplex) permission and
`ask_user` requests — so the zoomed view renders through the same transcript
pipeline as the top-level conversation. Parent-only events with no
subagent meaning (pursuit updates) are dropped on the way through.

### TUI zoom view

The `subagent` step renders inline as one summary line plus a live status line.
Pressing `Enter` on the step — or clicking it — pushes onto the view's focus
stack and the transcript switches to showing that step's children.

When zoomed in:

- The entire footer — plan panel, pursuit bar, status bar, input box, hint
  bar — is hidden. The subagent view is read-only chrome.
- A one-row navigation band at the bottom shows the position (`N of M`) on the
  left and `Esc back   [ prev   ] next` on the right.
- `Esc` pops back up the focus stack; `[` and `]` cycle sibling `subagent`
  steps at the current depth.

On `/resume`, persisted child transcripts repopulate the step's children, so the
zoom view rebuilds from disk. The live event stream always wins over the
snapshot. The detailed rendering reference is
[Subagent view](../../reference/tui/subagent-view.md).

### Failure and cancellation

A subagent that hits a harness safety bound (a read-only stall, repeated
identical calls) or a provider error still returns its result payload. Its
summary is prefixed so the failure classifier and the TUI's Failed badge both
trigger, and the partial transcript is preserved so the user can resume into the
half-finished work. Only input-validation errors (bad JSON, missing fields)
propagate as hard errors, because they have no partial transcript worth keeping.

The subagent runs with its own independent cancellation. When the parent turn is
interrupted, the parent simply stops awaiting the subagent and emits a
cancellation for the call id; the TUI then recursively cancels the nested tool
steps. No token needs to link the two — the parent dropping the future is
enough. The registry entry for the child is removed on return, so it never
holds a dead handle.

Real token usage from the subagent is accumulated into the parent turn's cost,
so it flows up to the active [pursuit](pursuits.md) if one is set.

## Plan verification

`verify_plan_execution` is the second subagent scenario — the mechanism behind
the "verify before declaring completion" instruction. It is documented as a tool
in [`verify_plan_execution`](../../reference/tools/plan.md#verify_plan_execution);
this section covers *why* it is a distinct subagent role.

### A second role, not a second `subagent`

The verifier runs through the same subagent plumbing — isolation, snapshot,
event forwarding, failure handling — but binds the [`VERIFY`](#profiles) profile
instead of `EXPLORE`. The difference is one axis: `VERIFY`'s access ceiling is
`Execute`, so the verifier additionally gets `bash` to run tests, builds, and
type-checks as concrete evidence — while still excluding file writes, user
questions, and recursion.

This is the scenario that forced the `Read < Execute < Write` tier split. An
independent auditor's most useful signal is behaviour — does it compile, do the
tests pass — not just "the code looks right". Static-only verification (what
`EXPLORE` gives) cannot produce that signal. But handing the verifier a
`Write`-ceiling profile would let it edit the implementation it is auditing,
which defeats independence. `Execute` is the tier between them: command
execution without file-write capability. See
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

### Clean role/task separation

The verifier's *role* contract — independent, unbiased, may run commands, must
not edit, non-interactive — lives in the `VERIFY` profile's system prompt. The
*task* — which plan to read, the PASS/PARTIAL/FAIL report format, the final
verdict line — is carried in the call's user prompt. Adding a new kind of
verification (a different report shape, a focused scope) is a different user
prompt against the same profile, not a new subagent.

## See also

- [`subagent`](../../reference/tools/subagent.md) — parameter reference.
- [Plan](plan.md) — the `PLAN` subagent and the plan workflow.
- [Turns and rounds](turns-and-rounds.md) — the round trip the subagent runs
  internally.
- [Pursuits](pursuits.md) — how subagent token cost flows up to a parent
  pursuit.
- [Harness architecture](harness.md) — the safety bounds that bound a subagent
  turn.
- [ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md) — the
  `WriteScope` grant.
- [ADR-0029](../../adr/0029-full-duplex-subagent-communication.md) — full-duplex
  communication.
