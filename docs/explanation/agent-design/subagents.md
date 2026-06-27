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
| `EXPLORE` | `subagent` tool | `Read` | none | Pure read tools (`read_file`, `grep`, `glob`, `list_dir`, …) |
| `REVIEW` | harness session-review diagnostic | `Read` | none | Pure read tools, run on a transcript snapshot |
| `TITLE` | harness title generation | `Read` | none | No tools — a single `provider.chat()` call |
| `INTERACTIVE` | (reserved, no dispatch tool yet) | `Read` | none | Pure read tools, with `ask_user` forwarded up |

All four are non-recursive (recursion is excluded absolutely, not per-profile
— see [Tool admission](#tool-admission)). Only `EXPLORE` is reachable from a
model tool call today; `REVIEW`, `TITLE`, and `INTERACTIVE` are internal
roles. `INTERACTIVE` opts into `allow_user_interaction: true` so an
`ask_user` request surfaces to the parent through the full-duplex channel; it
is defined ahead of a dispatch tool that needs it.

### Why a `Read` ceiling

The research role is pure inspection, no side effects. A researcher should not
run commands — an exploration subagent with `bash` could mutate the workspace
or run arbitrary commands, which is wrong for "go find things and report
back". Every built-in profile therefore carries a `Read` ceiling. The
`Read < Execute < Write` tier split (ADR-0012) and the decoupled `write_paths`
grant (ADR-0028) remain available for a future command-running or
scoped-write role, but no built-in profile exercises them today.

### Extending

Adding a new role is a new profile constant plus a binding at the dispatch
site — no orchestration surgery, no changes to the admission rule. An
interactive role (one whose `ask_user` requests are forwarded to the user)
already exists as `INTERACTIVE`; a future dispatch tool can bind it. The
full-duplex channel ([ADR-0029](#full-duplex)) already carries the request
and reply path. The profile primitive was introduced in
[ADR-0011](../../adr/0011-subagent-profiles.md) and extended with the tier
split in [ADR-0012](../../adr/0012-toolaccess-tier-split.md); the `PLAN` /
`VERIFY` profiles ADR-0027 / ADR-0012 once added were later removed
([ADR-0033](../../adr/0033-remove-plan-and-verify-workflow.md)).

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
| Filesystem access | Admitted when the tool's tier is at or below the ceiling — every built-in profile has a `Read` ceiling, so it drops `bash`/`Write` |
| Scoped write | A `Write` tool below the ceiling is admitted when `write_paths` is non-empty (then scoped at runtime). No built-in profile sets `write_paths` today. `Execute` is never granted this way |
| Needs a human | Excluded unless the profile opts in — `ask_user` and any future approval-gated tool |
| Spawns a subagent | Always excluded, in *every* profile — this is what prevents recursion |

### What falls out of the policy

- **Recursion is impossible.** `subagent` marks itself `spawns_subagent()`, so
  it is excluded from every subagent regardless of profile. No name list is
  involved — a new dispatch tool that declares the axis is covered
  automatically.
- **Scoped writes, not blanket writes.** A `Write` tool admitted by
  `write_paths` is then scoped at runtime by the agent's `WriteScope`, so a
  write anywhere outside the granted path is blocked at the execution funnel.
  Admission says *whether*; `WriteScope` says *where*. No built-in profile
  uses this today.
- **Pursuit and todo tools are inert.** They are added inside the subagent
  from a snapshot, tied to its own (empty) state cells — not the parent's.
  For a read-only research task they have nothing to act on.

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

The built-in profiles other than `INTERACTIVE` stay non-interactive, so in
practice no nested request is surfaced today and the child's
`set_unattended(true)` is a transitional gate rather than a load-bearing
deadlock fix. The `INTERACTIVE` profile opts into `allow_user_interaction`, so
its `ask_user` round-trip works through the handle directly.

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

- The entire footer — activity bar, input box, hint bar — is
  hidden. The subagent view is read-only chrome.
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

## See also

- [`subagent`](../../reference/tools/subagent.md) — parameter reference.
- [Turns and rounds](turns-and-rounds.md) — the round trip the subagent runs
  internally.
- [Pursuits](pursuits.md) — how subagent token cost flows up to a parent
  pursuit.
- [Harness architecture](harness.md) — the safety bounds that bound a subagent
  turn.
- [ADR-0011](../../adr/0011-subagent-profiles.md) — the capability-axis
  profile primitive.
- [ADR-0028](../../adr/0028-capability-allocation-scoped-writes.md) — the
  `WriteScope` grant.
- [ADR-0029](../../adr/0029-full-duplex-subagent-communication.md) — full-duplex
  communication.
- [ADR-0033](../../adr/0033-remove-plan-and-verify-workflow.md) — removal of
  the former `PLAN` / `VERIFY` profiles.
