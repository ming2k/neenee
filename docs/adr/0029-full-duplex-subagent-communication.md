# 0029. Full-duplex subagent communication (steering inbox + request/reply handle)

- **Status:** Accepted
- **Date:** 2026-06-25

## Context

Until now a subagent was fire-and-forget. The dispatch tool (`subagent`,
formerly `task`) ran the child to completion and returned its final answer; the
parent had no way to reach into a running child, and a child that surfaced a
`PermissionRequest` or `UserQuestionRequest` had no path up to the user — its
request was forwarded as a `SubagentEvent` but, with no reply channel, the
child's oneshot waited forever. ADR-0011 codified this as a hard rule:
subagents are non-interactive (`requires_user` tools excluded from every
profile), and `set_unattended(true)` suppresses the broker, precisely so an
unanswered prompt can never hang the child.

That made the whole subagent surface read-only and silent. It blocked two
things this codebase now wants:

1. **An interactive plan subagent.** ADR-0027's `PLAN` profile needs to
   clarify with the user mid-plan; the original ADR-0027 §5 routed
   clarification through the parent as a coarse round-trip (child returns "I
   need X", parent asks, re-spawns) because there was no live channel.
2. **Truthful permissioning of child side effects.** A subagent that writes or
   executes runs unattended today, which is a workaround for the missing
   reply path, not a real decision.

## Decision

Make subagent communication **full-duplex**: the child can surface interactive
requests *up* to the parent (and through it to the user), and the parent can
steer the child and resolve its requests *down*, while the child runs.

### Up direction — requests travel as `SubagentEvent`

`SubagentTool::forward_event` already wrapped the child's lifecycle events as
`SubagentEvent`. It now also forwards `PermissionRequest` and
`UserQuestionRequest` (previously dropped) so they reach the parent harness and
the TUI. The child's own broker/`ask_user` oneshot stays parked until a reply
arrives.

### Down direction — `SubagentHandle` + `SubagentRegistry`

Each child installs a steering inbox and exposes a handle the parent can act
through:

```rust
pub struct SubagentHandle {
    agent: Weak<Agent>,                 // cheap, can't keep a dead child alive
    steering: mpsc::UnboundedSender<AgentOp>,
}
```

Two capabilities, deliberately split:

| Capability | Method | Path | Timing |
|------------|--------|------|--------|
| **Steering** | `submit(AgentOp)` | inbox → `drain_inbox` | applied at the next tool-round boundary (never mid-tool) |
| **Request/reply** | `reply_permission` / `reply_user_question` | shared-state resolver directly | resolves the parked oneshot immediately |

Steering (e.g. "stop", "here is new context") is asynchronous and
boundary-safe: it queues onto the same channel as an external user turn and is
drained between tool rounds, so it can never interrupt a side effect mid-flight.
Request/reply is synchronous and direct: a permission/`ask_user` decision
already corresponds to a parked oneshot, so it resolves the waiter without
touching the inbox.

The dispatch tool keeps a `SubagentRegistry` mapping the **parent tool-call id**
to the live child's `SubagentHandle`. The child is registered when spawned and
removed when it returns, so the registry never accumulates dead handles. The
binary that constructs the tool exposes the same `Arc<SubagentRegistry>` to the
harness, so a user reply arriving on a nested `SubagentEvent::PermissionRequest`
can be routed back down: `registry.get(parent_call_id).reply_permission(...)`.

### What `set_unattended` becomes

With the reply path in place, the child's `set_unattended(true)` is no longer
a load-bearing deadlock fix — it is a **transitional gate**. The built-in
profiles (`EXPLORE`/`VERIFY`) still exclude `requires_user` tools, so in
practice no nested request is surfaced today; `unattended` only suppresses the
broker for the `Execute`-tier tools `VERIFY` admits (`bash`). A future
interactive profile (e.g. `PLAN` with `allow_user_interaction: true`) can drop
`unattended` and the round-trip just works through the handle.

## Alternatives considered

- **Keep subagents fire-and-forget; route clarification via parent round-trip
  (original ADR-0027 §5).** Rejected: a plan that needs several Q&A exchanges
  pays a full subagent spawn per exchange, and it leaves child side effects
  unattended forever. A live channel removes both costs.

- **Single bidirectional channel for everything (steering + replies).**
  Rejected: steering must be boundary-safe (never interrupt a tool side
  effect), while a permission reply must resolve *immediately* (the child is
  parked on a oneshot, not a queue). One channel cannot honour both timings.
  Splitting — async inbox for steering, direct resolver call for replies —
  matches the two distinct needs.

- **Strong reference (`Arc<Agent>`) in the handle.** Rejected: the registry
  would keep finished children alive. A `Weak` lets a completed child be freed
  on schedule; a late reply through an expired `Weak` degrades to a no-op
  rather than resurrecting or erroring.

- **Route replies through the inbox too.** Rejected: the child's broker/
  `ask_user` waiter is a `oneshot`, not an inbox consumer. Routing a reply
  through the inbox would require the child's loop to drain and match it,
  adding latency and a second dispatcher path. Resolving the oneshot directly
  is what the waiter already expects.

## Consequences

- **Positive:** `PLAN` can clarify inline (superseding ADR-0027 §5); a future
  interactive profile gets user interaction for free; child permissions can
  become truthful (the unattended gate can be dropped per-profile). The
  up/down split is general — any `SubagentEvent` the parent can render, the
  parent can now also answer.

- **Negative:** a new `AgentOp` inbox on the agent + a per-dispatch-tool
  registry to thread. Best-effort reply semantics (a late reply is a no-op)
  means a slow user can miss a child that already finished — acceptable, since
  the only consequence is the child proceeding without that input.

- **Neutral:** ADR-0011's "subagents are non-interactive" rule is relaxed at
  the mechanism level (the plumbing exists) but preserved at the profile level
  until a profile opts into `allow_user_interaction`. The default profiles are
  unchanged in behavior.

## References

- [ADR-0011](0011-subagent-profiles.md) — the capability-axis profile primitive
  the registry threads through; `allow_user_interaction` is now reachable.
- [ADR-0027](0027-plan-as-subagent.md) — §5 (clarification via parent
  round-trip) is superseded by this ADR's live channel.
- [ADR-0028](0028-capability-allocation-scoped-writes.md) — orthogonal; the
  `WriteScope` grant this builds on.
- [Subagents](../explanation/agent-design/subagents.md) — the duplex model.
