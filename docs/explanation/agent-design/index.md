# Agent design

This section is the design canon for neenee's agent — a bounded, tool-using,
semi-autonomous coding agent. The seven pages here are not independent
features; they are facets of one system. Read together they describe how a
single agent turn is steered, gated, isolated, made durable, and kept honest.

For where these docs sit relative to the rest of `docs/explanation/` — the
provider protocol layer and the terminal UI — see
[Explanation](../index.md).

## The recurring themes

The same handful of design ideas repeat across every page. Naming them up
front makes the individual docs easier to read, because each doc is a
variation on these themes rather than a one-off mechanism.

| Theme | What it means | Where it shows up |
|-------|---------------|-------------------|
| **Capability and access gating** | One permission surface (`ToolAccess`) feeds two gates: Plan mode and the permission broker. A tool declares `Read`/`Write` once; both gates consult it. | [Harness architecture](harness.md), [Plan mode](plan-mode.md), [MCP servers](mcp.md) |
| **Isolation boundaries** | Failure in one component must not topple the rest. Sub-agents are read-only; failed MCP servers are quarantined; goal state is per-thread. | [Sub-agents](subagents.md), [MCP servers](mcp.md), [Goals](goals.md) |
| **Durable vs ephemeral state** | The harness decides per concern what survives a restart. Goal identity and budgets are persisted in SQLite; the checklist is in-memory; sub-agent context is fresh per call. | [Goals](goals.md), [Sub-agents](subagents.md) |
| **Streaming and event propagation** | One event type (`AgentEvent`) flows from the agent through orchestration to the TUI; sub-agents re-emit the same shapes wrapped as `SubTaskEvent`. One pipeline renders everything. | [Sub-agents](subagents.md), [Harness architecture](harness.md) |
| **Fallback and degradation** | Every ideal path has a defined degradation: native tool calls fall back to text parsing; a missing MCP `inputSchema` defaults to `{"type":"object"}`; goal completion is deferred while checklist work remains. The system never silently relies on the happy path. | [Tool rounds](tool-rounds.md), [MCP servers](mcp.md), [Goals](goals.md) |
| **Control plane vs domain** | The harness owns steering (mode, goal, retry, loop); providers and tools own I/O. `TaskTool` lives in the agent crate because spawning a sub-agent is steering, not a domain action. | [Harness architecture](harness.md), [Sub-agents](subagents.md) |

## The canon, in reading order

A new contributor can read these top to bottom and end up with a complete
model of one agent turn.

1. [Harness architecture](harness.md) — the control plane around every
   provider call: turn execution, the two capability surfaces, the permission
   broker, the durable session, context compaction, and safety bounds. Start
   here.
2. [Tool rounds](tool-rounds.md) — the round trip of a tool call as a design
   concept: declaration, gating, execution, and how outcomes re-enter the
   conversation. This is the unit the rest of the canon operates on.
3. [Goals](goals.md) — durable per-session objectives: the status machine,
   the checklist that gates completion, token-budget enforcement in SQL, and
   the legacy migration. How the agent remembers what it is doing across
   turns and restarts.
4. [Plan mode](plan-mode.md) — a read-only execution surface for researching
   before editing. The cleanest example of capability gating: one `Read`/`Write`
   flag drives both the Plan-mode gate and the broker, with one deliberate
   exemption for plan files.
5. [Sub-agents](subagents.md) — the `task` tool's isolated read-only child
   agent. The reference for isolation: what is shared (the provider), what is
   fresh (history, goals, plan state), and how events stream back through one
   pipeline.
6. [MCP servers](mcp.md) — local stdio MCP servers as dynamically discovered
   tools. The reference for failure isolation and for how an extension surface
   reuses the same `Tool` trait and execution path as built-ins.
7. [User questions](user-questions.md) — the `ask_user` tool that blocks a turn
   to resolve ambiguity. The reference for the oneshot-channel blocking
   pattern the permission broker also uses.

## How a turn flows through the canon

A single agent turn touches almost every page. Tracing it is the fastest way
to see how the canon fits together:

```text
user message
  └─ [Harness] execute_turn: refresh system prompt (mode, goal, skills)
       └─ [Goals]  active goal + checklist injected into the prompt
       └─ [Provider] stream tokens; reconstruct native tool-call deltas
            └─ fallback? [Tool rounds] parse tool call from text
       └─ per tool call:
            ├─ [Plan mode] gate: allowed_in_plan_mode(arguments)?
            ├─ [Harness] permission broker (Write tools only)
            ├─ [Sub-agents] if call is `task`: spawn isolated child,
            │              stream SubTaskEvent back through the same pipeline
            ├─ [MCP]       if call is `mcp__*`: JSON-RPC over stdio
            └─ [User questions] if call is `ask_user`: block on oneshot
       └─ [Goals] account token/elapsed cost; maybe flip to budget_limited
       └─ completion marker? [Goals] defer unless checklist is clear
       └─ next tool round, or stop on final message / safety bound
```

Every arrow is documented in one of the canon pages above.

## Decision history

For the frozen rationale behind specific choices (why the progress panel, why
the strict layering, why plan mode v2), see the
[Architecture Decision Records](../../adr/). ADRs link back into this section
for background; this section links to ADRs for the decision trail.

## Adjacent layers

The canon describes the agent. Two other concerns live alongside it in
`docs/explanation/` and are intentionally outside this section:

- **Provider protocol** — [Request flow](../request-flow.md),
  [Provider capabilities](../provider-capabilities.md),
  [Guided decoding](../guided-decoding.md): the wire-level contract with model
  servers, SSE reassembly, and constrained decoding. These belong to the
  model-serving layer, not the agent.
- **Terminal UI** — [Terminal UI](../tui.md): how the semantic document model
  renders the canon's events to the screen.
