# Lifecycle hooks

A **lifecycle hook** is a user-configured action that runs automatically at a
specific point in the agent's lifecycle — before a tool call, after the model
tries to end a turn, when a session starts, before context is compacted. Hooks
let external practices (format-on-edit, a CI gate before completion, a
session-start notification, a "keep the test files" instruction folded into
compaction) attach to the agent as configuration, without touching the core
loop.

This page is the design deep dive. For where hooks sit in the control plane
see [Harness architecture](harness.md); for the events they share with other
mechanisms see [Tool rounds](turns-and-rounds.md), [Pursuits](pursuits.md), and
[Context compaction](context-compaction.md). For the configuration fields see
[Configuration Reference](../../reference/configuration.md#hooks); for the
decision history see [ADR-0025](../../adr/0025-lifecycle-event-hooks.md).

## Why hooks exist

The agent's lifecycle has a small set of natural interception points: a tool
is about to run, a tool just finished, the model is about to stop, a session
is opening or closing, the context is about to be summarized. Each is a place
where an external script could do useful work — enforce a rule, run a check,
inject a reminder, record an event.

Without hooks, every such practice earns its own code path through the core
loop. The result is exactly what neenee had before this design: a handful of
one-shot abstractions, each invented for a single job, each with one
implementation. Hooks replace that with one configurable surface: "when X
happens, run Y."

The distinguishing constraint is that hooks are **user-programmable
lifecycle points**, not a second copy of the engines that already drive the
agent. Context pressure, round counting, and the clock are each already owned
by a purpose-built mechanism — `CompactionPolicy` decides when to relieve
pressure, `/pursue` drives within-turn continuation, `/repeat` drives
clock-based work. Hooks do not re-expose those as configurable axes; they
expose the lifecycle events those engines fire on.

## One event axis, implicit capability

Hooks fire on **lifecycle events**, grouped by cadence into four families:

- **per session** — `SessionStart`, `SessionEnd`;
- **per turn** — `UserPromptSubmit`, `Stop`;
- **per tool call** — `PreToolUse`, `PostToolUse`, `PostToolUseFailure`;
- **per round** — `Round` (ADR-0030);
- plus the compaction pair `PreCompact` / `PostCompact`.

What a hook is *allowed to do* is not a knob the user picks — it is implied by
the event it fires on. A `PreToolUse` hook may block the call; a `Stop` hook
may force another round; a `PostToolUse` or `UserPromptSubmit` hook may
inject context the model then sees; the rest only observe. The user writes
"on `Stop`, run this script"; the system already knows a `Stop` hook's result
is a continue-or-stop decision.

This is deliberate. A design that exposed the context threshold, the round
count, and the clock as further hook axes would duplicate the engines that
already govern them and muddy two clean concerns — "what the harness does
under pressure or on a schedule" versus "what the user adds on a lifecycle
event." neenee keeps the first internal and exposes only the second.

## The event set

| Event | Fires | Capability |
|-------|-------|------------|
| `SessionStart` | A session begins or resumes | Observe; injected context becomes hidden setup messages |
| `SessionEnd` | A session ends on clean exit | Observe |
| `UserPromptSubmit` | The user submits a prompt, before it enters the transcript | Deny (drop the prompt) or inject (prepend context) |
| `PreToolUse` | Before a tool call runs | Deny (block the call) |
| `PostToolUse` | After a tool call succeeds | Inject context |
| `PostToolUseFailure` | After a tool call fails | Inject context |
| `Stop` | The model tries to end the turn | Deny (force another round, feeding the reason back) or inject |
| `PreCompact` | Before a summarizing compaction | Inject (folded into the summary prompt) |
| `PostCompact` | After a compaction completes | Observe |
| `Round` | Once per tool round (ADR-0030) | Inject only — **`Deny` is ignored**, so a round-count hook cannot become a de-facto round cap. Carries the read-only-round streak so a hook can target exploration-without-progress. The harness declares no built-in threshold here; users opt in. |

A hook returning a capability the event does not honour is ignored, so a
script that unconditionally reports a deny only bites on events that act on a
deny.

## Matchers

The three tool events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`)
filter on the tool name. A matcher is a plain string evaluated by its shape:

| Matcher shape | Evaluation | Example |
|---------------|------------|---------|
| Only letters, digits, `_`, and `|` | A pipe-separated list of exact names | `Write|Edit` matches either tool exactly |
| Any other character | A regular expression | `^Bash.*`, `mcp__.*` |
| Omitted or `*` | Matches every tool | — |

MCP tools surface as `mcp__<server>__<tool>` and match identically, so a
single `mcp__memory__.*` matcher covers every tool on the `memory` server.
The non-tool events ignore the matcher and fire on every occurrence.

## The command contract

A hook runs a shell command. The command receives a JSON snapshot of the
event on stdin and replies through its exit code and stdout:

```text
event fires, matcher matches
  └─ spawn  sh -c <command>,   cwd = project root
        stdin  ←  { "event", "session_id", "tool_name", ... }  (JSON)
  └─ within 60 s:
        exit 2 + stderr          → deny;  stderr is the reason fed back
        stdout is a JSON object  → { "decision": "deny"|"approve",
                                     "reason": "...", "context": "..." }
        anything else            → pass (a non-blocking error never aborts)
```

The three reply shapes map to the three capabilities: `decision: "deny"` (or
exit 2) blocks or continues depending on the event; `context` injects text;
anything else passes. A hook that times out, fails to spawn, or exits
non-zero with no decision JSON is treated as pass — a flaky script cannot
wedge the agent loop. Hard rules belong to the
[permission system](harness.md), not a hook.

The JSON object is flat and `jq`-friendly: one level with `event`,
`session_id`, `cwd`, and the event-specific fields (`tool_name`,
`tool_input`, `tool_output`, `prompt`, `last_message`, …).

## Composition with the loop

Hooks do not replace the agent's existing gates; they sit alongside them.

A tool call flows through several gates in order. A `PreToolUse` hook runs
first — before the permission broker is even asked — so a hook can spare the
user a permission prompt for a call it intends to block:

```text
tool call declared
  ├─ [Hooks]        PreToolUse (matcher?)  ── deny? → blocked, reason to model
  ├─ [WriteScope]   per-agent write boundary (subagents only)
  ├─ [Harness]      permission broker (Write / Execute tools)
  ├─              tool executes
  └─ [Hooks]      PostToolUse (success) | PostToolUseFailure (error)
                        └─ inject context? → hidden message on the next round
```

At turn end, the `Stop` hook composes with the `/pursue` stop-gate. The
pursuit gate is queried first (it owns its safety cap and completion signal);
only when the pursuit lets the turn stop do `Stop` hooks get a vote. The turn
ends only when **both** agree to stop. A `Stop` hook that denies forces one
more round with its reason fed back as a hidden user message — the same shape
a pursuit uses, just driven by an external script instead of a condition.

Around compaction, a `PreCompact` hook's injected context is folded into the
summary prompt (so a hook can say "prefer keeping the test files" and have it
influence what the model summarizes), and `PostCompact` observes the result.

## What hooks are not

- **Not a threshold or time axis, and only a constrained round axis.** Context
  pressure and the clock stay internal (`CompactionPolicy`, `/repeat`). Round
  counting is exposed as a single `Round` event (ADR-0030) but **`Deny`-forbidden**
  — it lets a hook inject context at a round boundary (e.g. to react to a
  read-only streak) without being able to abort the turn, which would recreate
  the blanket round cap ADR-0009 removed. The harness sets no built-in
  threshold on it; only the user does, at their own risk.
- **Not a substitute for permissions.** A hook deny is best-effort and
  non-fatal on failure; the permission broker is the hard enforcement
  surface. Enforce mandatory policy with permissions, use hooks for
  project-specific practice.
- **Not synchronous with the model.** A hook runs between rounds or before a
  call; it does not pause generation. Long work should be offloaded (a hook
  can itself spawn detached processes); the 60-second bound keeps the loop
  responsive.

## See also

- [Harness architecture](harness.md) — the control plane the hooks attach to,
  and the permission broker a `PreToolUse` hook precedes
- [Tool rounds](turns-and-rounds.md) — the tool-call round trip the per-tool events
  bracket
- [Pursuits](pursuits.md) — the `/pursue` stop-gate a `Stop` hook composes
  with at turn end
- [Context compaction](context-compaction.md) — the summarization the
  `PreCompact` / `PostCompact` events surround
- [Configuration Reference](../../reference/configuration.md#hooks) — the
  `[[hooks]]` table fields
- [ADR-0025](../../adr/0025-lifecycle-event-hooks.md) — the decision to
  adopt a single event axis with implicit capability, and the multi-axis
  design rejected along the way
- [ADR-0030](../../adr/0030-early-loop-intervention-and-round-hook.md) — the
  `Deny`-forbidden `Round` event that partially supersedes ADR-0025's exclusion
  of round-count (the in-loop review nudge it also added was later reworked into
  the deterministic guard of [ADR-0034](../../adr/0034-range-aware-pruning-and-deterministic-read-loop-guard.md))
