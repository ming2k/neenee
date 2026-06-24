# Sub-agents

The `task` tool spawns an isolated child agent to investigate a sub-question
and return a written answer. The parent agent stays in control of all writes
and of any questions to the user. For the tool's parameters and access class,
see [`task`](../../reference/tools/task.md).

## Why a sub-agent tool

A single agent turn accumulates context: every file read, every grep result,
every tool round stays in the transcript. For a large investigation that
touches many unrelated corners of the codebase, one of two things happens —
either the context fills with material only loosely related to the final
answer, or the model spends turns re-reading things it already saw. A
sub-agent gives the model a way to delegate the exploration:

1. **Context isolation.** The sub-agent runs with a fresh two-message history
   (its task prompt plus the system prompt). Its tool rounds never enter the
   parent's transcript; only its final summary does.
2. **Read-only and non-interactive by construction.** The sub-agent receives
   only the tools its profile admits, so it cannot mutate the workspace, never
   triggers the permission broker, and can never block on a question to the
   user (it has no user to reach). See [Profiles](#profiles) and
   [Tool admission](#tool-admission).
3. **Parallelizable investigation.** The model can dispatch several `task`
   calls to map different parts of a problem, then act on the synthesized
   findings.

## The `task` tool

The `task` tool is the one built-in tool whose result is not a single value
but a streamed investigation. It takes a short description and a prompt, both
required, and returns a payload carrying the sub-agent's summary, its full
transcript, and its token usage. The parent persists that transcript as the
tool step's children, so `/resume` rebuilds the nested view later.

Because the sub-agent's progress is interesting in real time (not just its
final answer), the tool streams live rather than blocking until completion:
every token and tool round the child produces is relayed to the parent TUI as
it happens. Input validation rejects only non-JSON or empty-after-trim fields;
the length hint on the description is a model-facing nudge, not an enforced
bound.

## Isolation model

The sub-agent shares exactly one thing with the parent — the model provider —
and nothing else:

| Concern | Shared? | How |
|---------|---------|-----|
| Provider | Yes | The same provider connection |
| Conversation history | No | A fresh system + task prompt |
| Tools | Snapshot, profile-filtered | The tools the bound [profile](#profiles) admits |
| Pursuit state | No | An empty in-memory pursuit store |
| Plan state, mode | No | Build mode, no active plan |
| Skills | No | No loaded skills |
| Cancellation token | No | A fresh, independent token |
| Session persistence | No | The sub-agent is never persisted |

The filesystem is implicitly shared because the sub-agent inherits the process
working directory, but its profile admits no file-write tools, so it cannot
mutate files.

## Profiles

A sub-agent's behaviour is not hardcoded in the dispatch tool. It is the output
of a declarative **profile** — a name, a system-prompt fragment that frames the
role, and a [`ToolPolicy`](#tool-admission) that scopes what it may touch.
Profiles are domain vocabulary; the dispatch tools bind them by reference.

### The two built-in profiles

| Profile | Bound by | Access ceiling | Gets |
|---------|----------|----------------|------|
| `EXPLORE` | `task` | `Read` | Pure read tools (`read_file`, `grep`, `glob`, `list_dir`, …) |
| `VERIFY` | `verify_plan_execution` | `Execute` | Read tools **plus `bash`** for tests/builds/type-checks |

Both are non-interactive (`allow_user_interaction: false`) and non-recursive
(recursion is excluded absolutely, not per-profile — see
[Tool admission](#tool-admission)). The profile is the single source of truth;
the dispatch tool takes the profile explicitly, and the verifier path goes
through the same dispatch tool.

### Why two roles instead of one

`EXPLORE` is the research role: pure inspection, no side effects. A researcher
should not run commands — an exploration sub-agent with `bash` could mutate
the workspace or run arbitrary commands, which is wrong for "go find things
and report back".

`VERIFY` is the independent-auditor role. An auditor's most valuable evidence
is *behaviour*: does `cargo test` pass, does it build, does it type-check?
Static inspection alone cannot answer those. So the verifier needs command
execution. But it must still not edit the implementation it is auditing — an
independent auditor that can rewrite the thing it is checking is not
independent.

The two needs (no commands vs. commands-but-no-file-writes) cannot be
expressed by a single Read/Write ceiling. Resolving that is what the
`Read < Execute < Write` tier split is for: `VERIFY`'s `Execute` ceiling
admits `bash` while still excluding `write_file`/`edit_file` (`Write`). See
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

### Extending

Adding a third role is a new profile constant plus a binding at the dispatch
site — no orchestration surgery, no changes to the admission rule. A future
write-capable "executor" role, or an interactive role (one where question
requests are genuinely forwarded to the user), would land here. The profile
primitive was introduced in [ADR-0011](../../adr/0011-subagent-profiles.md)
and extended to two roles + the tier split in
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

## Tool admission

How a profile decides which of the parent's tools a sub-agent may actually
use. The profile primitive and the two built-in roles are covered in
[Profiles](#profiles); this section is the per-tool decision rule and the
rationale for the exclusions.

### The admission check

Each profile carries a `ToolPolicy` with an `access` ceiling and an
`allow_user_interaction` flag. Admission checks three capability axes on each
tool. The access axis is an ordered ceiling (`Read < Execute < Write`); the
other two are gates:

| Axis | Rule |
|------|------|
| Filesystem access | Admitted when the tool's access tier is at or below the profile's ceiling — so `EXPLORE` drops `bash`/`Write`, `VERIFY` drops only `Write` |
| Needs a human | Excluded unless the profile opts in — `ask_user` and any future approval-gated tool |
| Spawns a sub-agent | Always excluded, in *every* profile — this is what prevents recursion |

### What falls out of the policy

- **Recursion is impossible.** `task` marks itself `spawns_subagent()`, so it
  is excluded from every sub-agent regardless of profile.
  `verify_plan_execution` does the same. No name list is involved — a new
  dispatch tool that declares the axis is covered automatically.
- **The sub-agent cannot hang on the user.** `ask_user` is `Read` but
  requires a human, so both built-in profiles exclude it. A sub-agent has no
  user reachable — its question-request events are dropped by the dispatch
  tool's forwarder — so admitting `ask_user` would deadlock until the parent
  turn is cancelled. Excluding it by capability is what closes that hole. The
  forwarder still has a defensive `tracing::error!` arm in case a future
  interactive tool leaks past a profile, so an invariant break is observable
  rather than turning into a silent hang.
- **The verifier can run tests, but not edit the answer.** `VERIFY`'s
  `Execute` ceiling admits `bash` (so `cargo test` / builds / type-checks
  count as evidence) but still drops `write_file`/`edit_file` — an
  independent auditor must not mutate the implementation it is auditing.
- **Pursuit, plan, and verify tools are inert.** They are added inside the
  sub-agent from a snapshot, tied to its own (empty) state cells — not the
  parent's. For a read-only research task they have nothing to act on.

### The snapshot

The parent toolset is snapshotted once when the dispatch tool is constructed,
after built-ins and MCP tools are assembled and before later additions (the
dispatch tool itself, the history tool). Read-only MCP servers are therefore
visible to sub-agents; tools assembled later are not. The profile then filters
that snapshot — so admission has two stages (snapshot membership, then
`admits`), and both must pass.

### Why capability axes, not a name list

An earlier design filtered by access tier plus a name exclusion (`Read` and
not `task`). That was name-driven and missed `ask_user` (which is `Read`),
so the sub-agent could call it and deadlock. The capability-axis model makes
each exclusion semantic: a tool is excluded because of *what it does* (blocks
on a human, spawns an agent, mutates the workspace), not because of what it is
called. A future interactive or dispatch tool is covered the moment it
declares its axis. See [ADR-0011](../../adr/0011-subagent-profiles.md).

## Runtime

How a sub-agent executes once dispatched: the event stream it emits, how the
TUI renders it, how it fails, and how it behaves across Plan/Build mode.

### Event streaming

The sub-agent is a real agent, so it emits the full event stream a parent
does. Each child event is wrapped and forwarded to the parent, so the TUI
builds the nested view in real time:

```text
sub-agent event ──forward──► wrapped child event
                                │
parent dispatch        ──► parent event carrying the child event
                                │
orchestration relay    ──► response the TUI appends to the matching step
```

The forwarded events carry the same shapes the parent stream does — streaming
deltas, tool calls, tool results, activity — so the zoomed view renders
through the same transcript pipeline as the top-level conversation.
Parent-only events that have no read-only-researcher meaning (pursuit
updates, mode changes, permission requests) are dropped on the way through. A
question request that somehow reaches the forwarder is dropped with a
defensive error log rather than silently deadlocking (see
[Tool admission](#tool-admission)).

### TUI zoom view

The `task` step renders inline as one summary line plus a live status line.
Pressing `Enter` on the step — or clicking it — pushes onto the view's focus
stack and the transcript switches to showing that step's children.

When zoomed in:

- The entire footer — plan panel, pursuit bar, status bar, input box, hint
  bar — is hidden. The sub-agent view is read-only chrome.
- A one-row navigation band at the bottom shows the task position (`N of M`)
  on the left and `Esc back   [ prev   ] next` on the right.
- `Esc` pops back up the focus stack; `[` and `]` cycle sibling `task` steps
  at the current depth.
- The plan progress panel is hidden, because the plan belongs to the parent
  context (see [Plan mode](plan-mode.md)).

On `/resume`, persisted child transcripts repopulate the step's children, so
the zoom view rebuilds from disk. The live event stream always wins over the
snapshot. The detailed rendering reference is
[Sub-agent view](../../reference/tui/subagent-view.md).

### Failure and cancellation

A sub-agent that hits a harness safety bound (a read-only stall, repeated
identical calls) or a provider error still returns its result payload. Its
summary is prefixed so the existing failure classifier and the TUI's Failed
badge both trigger, and the partial transcript is preserved so the user can
resume into the half-finished work. Only input-validation errors (bad JSON,
missing fields) propagate as hard errors, because they have no partial
transcript worth keeping.

The sub-agent runs with its own independent cancellation. When the parent
turn is interrupted, the parent simply stops awaiting the sub-agent and emits
a cancellation for the `task` call id; the TUI then recursively cancels the
nested tool steps. No token needs to link the two — the parent dropping the
future is enough.

Real token usage from the sub-agent is accumulated into the parent turn's
cost, so it flows up to the active [pursuit](pursuits.md) if one is set.

### Plan mode

`task` is a read-access tool, so the default Plan-mode rule permits it, and
the Plan-mode system prompt explicitly endorses it as a read-only research
tool.

The sub-agent is Build mode regardless of the parent's mode. This is not a
tension: the Plan-mode gate only restricts above-`Read` tools, and the
sub-agent never holds the parent's mode handle anyway. Whether the parent is
in Plan or Build, the sub-agent behaves as its profile dictates — a
read-only researcher (`EXPLORE`) or a command-running verifier (`VERIFY`).

## Plan verification

`verify_plan_execution` is the second sub-agent scenario — the mechanism
behind the Build-mode prompt's "spawn a verifier before declaring completion"
instruction. It is documented as a tool in
[`verify_plan_execution`](../../reference/tools/plan.md#verify_plan_execution);
this section covers *why* it is a distinct sub-agent role.

### A second role, not a second `task`

The verifier constructs its own `TaskTool` (so it reuses all the sub-agent
plumbing — isolation, snapshot, event forwarding, failure handling) but binds
the [`VERIFY`](#profiles) profile instead of `EXPLORE`. The difference is one
axis: `VERIFY`'s access ceiling is `Execute`, so the verifier additionally
gets `bash` to run tests, builds, and type-checks as concrete evidence — while
still excluding file writes, user questions, and recursion.

This is the scenario that forced the `Read < Execute < Write` tier split. An
independent auditor's most useful signal is behaviour — does it compile, do
the tests pass — not just "the code looks right". Static-only verification
(what `EXPLORE` gives) cannot produce that signal. But handing the verifier a
`Write`-ceiling profile would let it edit the implementation it is auditing,
which defeats independence. `Execute` is the tier between them: command
execution without file-write capability. See
[ADR-0012](../../adr/0012-toolaccess-tier-split.md).

### Clean role/task separation

The verifier's *role* contract — independent, unbiased, may run commands,
must not edit, non-interactive — lives in the `VERIFY` profile's system
prompt. The *task* — which plan to read, the PASS/PARTIAL/FAIL report format,
the final verdict line — is carried in the call's user prompt. Adding a new
kind of verification (a different report shape, a focused scope) is a
different user prompt against the same profile, not a new sub-agent.

### Non-streaming by design

Unlike `task`, the verifier uses the non-streaming call path, so its nested
step does not stream live tokens. A verifier reports a final verdict rather
than an investigation to watch; streaming its token-by-token reasoning would
add noise without adding signal.

## See also

- [`task`](../../reference/tools/task.md) — parameter reference.
- [Plan mode](plan-mode.md) — `task` in Plan mode, and plan verification.
- [Turns and rounds](turns-and-rounds.md) — the round trip the sub-agent runs
  internally.
- [Pursuits](pursuits.md) — how sub-agent token cost flows up to a parent
  pursuit.
- [Harness architecture](harness.md) — the safety bounds that bound a
  sub-agent turn.
