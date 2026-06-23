# Sub-agent runtime

How a sub-agent executes once dispatched: the event stream it emits, how the
TUI renders it, how it fails, and how it behaves across Plan/Build mode.

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
deltas, tool calls, tool results, activity — so the zoomed view renders through
the same transcript pipeline as the top-level conversation. Parent-only events
that have no read-only-researcher meaning (pursuit updates, mode changes,
permission requests) are dropped on the way through. A question request
that somehow reaches the forwarder is dropped with a defensive error log
rather than silently deadlocking (see
[Admission](admission.md)).

## TUI zoom view

The `task` step renders inline as one summary line plus a live status line.
Pressing `Enter` on the step — or clicking it — pushes onto the view's focus
stack and the transcript switches to showing that step's children.

When zoomed in:

- The entire footer — plan panel, pursuit bar, status bar, input box, hint bar —
  is hidden. The sub-agent view is read-only chrome.
- A one-row navigation band at the bottom shows the task position (`N of M`)
  on the left and `Esc back   [ prev   ] next` on the right.
- `Esc` pops back up the focus stack; `[` and `]` cycle sibling `task` steps
  at the current depth.
- The plan progress panel is hidden, because the plan belongs to the parent
  context (see [Plan mode](../plan-mode.md)).

On `/resume`, persisted child transcripts repopulate the step's children, so
the zoom view rebuilds from disk. The live event stream always wins over the
snapshot. The detailed rendering reference is
[Sub-agent view](../../../reference/tui/subagent-view.md).

## Failure and cancellation

A sub-agent that hits a harness safety bound (a read-only stall, repeated
identical calls) or a provider error still returns its result payload. Its
summary is prefixed so the existing failure classifier and the TUI's Failed
badge both trigger, and the partial transcript is preserved so the user can
resume into the half-finished work. Only input-validation errors (bad JSON,
missing fields) propagate as hard errors, because they have no partial
transcript worth keeping.

The sub-agent runs with its own independent cancellation. When the parent turn
is interrupted, the parent simply stops awaiting the sub-agent and emits a
cancellation for the `task` call id; the TUI then recursively cancels the
nested tool steps. No token needs to link the two — the parent dropping the
future is enough.

Real token usage from the sub-agent is accumulated into the parent turn's cost,
so it flows up to the active [pursuit](../pursuits.md) if one is set.

## Plan mode

`task` is a read-access tool, so the default Plan-mode rule permits it, and the
Plan-mode system prompt explicitly endorses it as a read-only research tool.

The sub-agent is Build mode regardless of the parent's mode. This is not a
tension: the Plan-mode gate only restricts above-`Read` tools, and the
sub-agent never holds the parent's mode handle anyway. Whether the parent is in
Plan or Build, the sub-agent behaves as its profile dictates — a read-only
researcher (`EXPLORE`) or a command-running verifier (`VERIFY`).

## See also

- [Profiles](profiles.md) and [Admission](admission.md) — what the sub-agent
  may do.
- [Harness architecture](../harness.md) — the safety bounds that bound a turn.
- [Sub-agent view](../../../reference/tui/subagent-view.md) — the TUI reference.
