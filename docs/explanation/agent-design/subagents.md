# Sub-agents

The `task` tool spawns an isolated, read-only child agent to investigate a
sub-question and return a written answer. The parent agent stays in control of
all writes. This page covers the isolation model, event streaming, and the TUI
zoom view. For the tool's parameters and access class, see
[Built-in tools](../../reference/tools.md).

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
2. **Read-only by construction.** The sub-agent receives only read-access
   tools, so it cannot mutate the workspace, and it never triggers the
   permission broker.
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
it happens.

Input validation rejects only non-JSON or empty-after-trim fields. The length
hint on the description is a model-facing nudge, not an enforced bound.

## Isolation model

The sub-agent shares exactly one thing with the parent — the model provider —
and nothing else:

| Concern | Shared? | How |
|---------|---------|-----|
| Provider | Yes | The same provider connection |
| Conversation history | No | A fresh system + task prompt |
| Tools | Snapshot, filtered | The read-only subset (see below) |
| Goal state | No | An empty in-memory goal store |
| Plan state, mode | No | Build mode, no active plan |
| Skills | No | No loaded skills |
| Cancellation token | No | A fresh, independent token |
| Session persistence | No | The sub-agent is never persisted |

The filesystem is implicitly shared because the sub-agent inherits the process
working directory, but its toolset has no write tools, so it cannot mutate
files.

### Tool filtering

The sub-agent runs with the read-only subset of the parent's tools. Two
consequences fall out of that filter alone:

- **Recursion is impossible.** `task` itself is excluded, so a sub-agent
  cannot spawn another sub-agent.
- **Goal, plan, and verify tools are inert.** They are added inside the
  sub-agent from a snapshot, tied to its own (empty) state cells — not the
  parent's. For a read-only research task they have nothing to act on.

The snapshot is captured once, after built-ins and MCP tools are assembled.
Read-only MCP servers are therefore visible to sub-agents; tools assembled
later, such as the history tool, are not.

## Event streaming

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
Parent-only events that have no read-only-researcher meaning (goal updates,
mode changes, permission requests) are dropped on the way through.

## TUI zoom view

The `task` step renders inline as one summary line plus a live status line.
Pressing `Enter` on the step — or clicking it — pushes onto the view's focus
stack and the transcript switches to showing that step's children.

When zoomed in:

- The entire footer — plan panel, goal bar, status bar, input box, hint bar —
  is hidden. The sub-agent view is read-only chrome.
- A one-row navigation band at the bottom shows the task position
  (`N of M`) on the left and `Esc back   [ prev   ] next` on the right.
- `Esc` pops back up the focus stack; `[` and `]` cycle sibling `task` steps
  at the current depth.
- The plan progress panel is hidden, because the plan belongs to the parent
  context (see [Plan mode](plan-mode.md)).

On `/resume`, persisted child transcripts repopulate the step's children, so
the zoom view rebuilds from disk. The live event stream always wins over the
snapshot.

## Failure and cancellation

A sub-agent that hits a harness safety bound (too many tool rounds, repeated
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
cost, so it flows up to the active [goal](goals.md) if one is set.

## Plan mode

`task` is a read-access tool, so the default Plan-mode rule permits it, and
the Plan-mode system prompt explicitly endorses it as a read-only research
tool.

The sub-agent is Build mode regardless of the parent's mode. This is not a
tension: the Plan-mode gate only matters for write tools, and the sub-agent
has none. Whether the parent is in Plan or Build, the sub-agent behaves as a
read-only researcher.

## Related: plan verification

There is a sibling tool that reuses `task` internally to spawn an independent
verifier with a clean context — the mechanism behind the Build-mode prompt's
"spawn a verifier before declaring completion" instruction. It uses the
non-streaming path, so its nested step does not stream live tokens, by design:
a verifier reports a final PASS / PARTIAL / FAIL verdict rather than an
investigation to watch. See [Plan mode](plan-mode.md).

## See also

- [Built-in tools](../../reference/tools.md) — `task` parameter schema and
  access class
- [Plan mode](plan-mode.md) — `task` in Plan mode, and plan verification
- [Tool rounds](tool-rounds.md) — the round trip the sub-agent runs internally
- [Goals](goals.md) — how sub-agent token cost flows up to a parent goal
- [Harness architecture](harness.md) — the safety bounds that bound a
  sub-agent turn
