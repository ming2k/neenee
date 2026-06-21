# Turns and rounds

neenee executes a request in two nested layers. A **turn** is the unit the
user perceives: one submitted message, one final reply. A **round** is the
unit the ReAct loop iterates on inside that turn: one model request, plus
the tool work that follows when the model asks for it. One turn is one or
many rounds; one round never spans turns.

The split is not decorative. Different concerns attach to each layer, and
keeping them straight is the key to reading the rest of the canon. For the
control plane that drives a turn, see [Harness architecture](harness.md).
For what happens *inside* a single round, see [Tool rounds](tool-rounds.md).

## The two layers

```text
turn  ────────────────────────────────────────────────┐
  │                                                   │
  ├── round 1: model request → tool call → result ──┐ │
  ├── round 2: model request → tool call → result ──┤ │
  ├── round 3: model request → tool call → result ──┤ │
  └── round N: model request → final text (no call) │ │
                                                    │ │
  turn ends ────────────────────────────────────────┘ │
                                                     │
next turn ───────────────────────────────────────────┘
```

A **turn** opens when the user submits a message and closes when the agent
produces a final assistant message that carries no tool call. Everything
between — every model request, every tool execution, every result folded
back into the transcript — belongs to that one turn.

A **round** is one pass through that loop: send the conversation to the
model, let the response commit, and either execute the tool calls it
carries (then loop) or treat it as the turn's answer (then stop). A
trivial turn that needs no tools is a single round. A turn that reads,
edits, and verifies may run several.

The round counter resets at the start of every turn. A separate,
monotonic **turn counter** persists across turns for the concerns that
need to measure passage between turns — plan staleness, goal accounting.

## What ends a turn

A turn stops on the first of these conditions:

| Condition | Kind | What the user sees |
|-----------|------|--------------------|
| Final assistant message with no tool call | Natural completion | The reply |
| Repeated-call guard trips | Stuck loop | An error |
| User interrupt | Cancellation | The turn stops where it is |
| Permission denied | Abort | The denied call's result ends the loop |

There is **no per-turn round cap**: distinct tool calls are allowed to run
uncapped, matching the codex / claude-code agentic-loop model (ADR-0009).
Context compaction is the backstop that keeps long turns bounded; the user
can interrupt at any time. The repeated-call guard is the only in-loop
guardrail: three identical calls in a row mean the loop is stuck, so the
fourth is rejected as an error rather than silently swallowed.

For the rest of the safety surface, see [Harness architecture](harness.md).

## What ends a round

A round ends when the model's response commits — when the stream
terminates and the assistant message is final. Up to that boundary,
nothing with side effects has run; the round is still retryable. Once the
response commits, the turn either executes the tool calls it carries and
starts a new round, or treats the message as the answer and ends the turn.

The lifecycle inside one round — declaration, gating, execution, and how
the outcome re-enters the transcript — is the subject of
[Tool rounds](tool-rounds.md).

## Why two layers

The layers exist because the concerns that govern an agent run attach to
different granularities. Measuring everything at the turn level is too
coarse: a single turn can burn the whole context budget if nothing watches
the loop body. Measuring everything at the round level is too fine: the
user does not perceive rounds, and durability that changed mid-loop would
be incoherent.

| Concern | Layer | Why it lives there |
|---------|-------|--------------------|
| Repeated-call guard | Round | A stuck loop is unbounded by default; the guardrail watches each iteration for the one signature of "stuck" (same name + args) |
| Mid-turn context relief | Round | Pruning old tool results between rounds reclaims space before the next request, inside one turn |
| Pre-tool retry safety | Round | A round is retryable until its first side effect; after that, retry is terminal |
| Goal token and time accounting | Turn | Cost is booked once the turn's outcome is final, not partway through |
| Plan staleness | Turn | "Turns since the plan was last updated" is the signal that the model has drifted |
| Session durability | Turn | The transcript commits at the turn boundary, never mid-loop |
| Autonomous loop budget | Turn | `/loop` counts completed turns (iterations) for status display and durable resume — uncapped, see ADR-0009 |

The rule of thumb: if a concern watches the loop body, it is round-scoped;
if it books a result or measures passage of work, it is turn-scoped.

## How the layers show up

The round layer is visible to the user only as live progress. While a turn
runs, the activity bar reports both layers as a structural prefix —
`turn N · round M · <status>` — so the user can see at a glance how far into
the turn the loop has gone. Each tool call renders as a step. When the turn
ends, the round detail collapses into the single user-visible exchange.

The turn layer is the durable shape of the conversation. The transcript is
a sequence of turns; goal accounting, plan progress, and the persisted
session all advance at turn boundaries. A resumed session restores whole
turns, never partial rounds.

A sub-agent runs its own turn with its own independent round budget — the
parent's round counter does not move while the child works. See
[Sub-agents](subagents/index.md).

## A turn of several rounds

A user asks: *fix the bug in `parser.rs` and explain the fix*. One turn,
four rounds:

```text
turn opens
  round 1  read_file(parser.rs)   ← model inspects
  round 2  edit_file(parser.rs)   ← model applies the fix
  round 3  read_file(parser.rs)   ← model verifies the result
  round 4  "The bug was …"        ← final text, no tool call
turn ends
```

Each round sends the full, growing transcript back to the model; the
conversational memory is the transcript the agent resends, not anything
the model remembers. If the transcript grows past the context budget
mid-turn, relief prunes old tool results between rounds — the turn does
not have to end to reclaim space. When round 4 produces plain text with no
tool call, the turn closes, goal accounting books the combined token cost
of all four rounds, and the plan's staleness counter advances by one turn.

## See also

- [Harness architecture](harness.md) — turn execution, retry, and the
  full table of safety bounds
- [Tool rounds](tool-rounds.md) — the lifecycle of one round: declaration,
  gating, execution, and re-entry into the transcript
- [Goals](goals.md) — turn-scoped token and time accounting
- [Plan mode](plan-mode.md) — plan staleness measured in turns
- [Sub-agents](subagents/index.md) — independent turns and round budgets for
  child agents
