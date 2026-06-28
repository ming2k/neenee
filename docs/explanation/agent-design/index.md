# Agent design

This section is the design canon for neenee's agent — a bounded, tool-using,
semi-autonomous coding agent. The pages here are not independent
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
| **Capability and access gating** | One permission surface (`ToolAccess`, ordered `Read < Execute < Write`) feeds two gates: the per-agent `WriteScope` boundary and the permission broker. A tool declares its access tier once; both gates consult it. | [Harness architecture](harness.md), [Turns and rounds](turns-and-rounds.md), [MCP servers](mcp.md) |
| **Isolation boundaries** | Failure in one component must not topple the rest. Sub-agents are read-only; failed MCP servers are quarantined; pursuit state is per-thread. | [Sub-agents](subagents.md), [MCP servers](mcp.md), [Pursuits](pursuits.md) |
| **Durable vs ephemeral state** | The harness decides per concern what survives a restart. The durable session preserves the recoverable scene; the model context is a request-scoped projection; sub-agent context is fresh per call. | [Session persistence](session-persistence.md), [Model context](model-context.md), [Sub-agents](subagents.md) |
| **Streaming and event propagation** | One event type (`AgentEvent`) flows from the agent through orchestration to the TUI; sub-agents re-emit the same shapes wrapped as `SubTaskEvent`. One pipeline renders everything. | [Sub-agents](subagents.md), [Harness architecture](harness.md) |
| **Fallback and degradation** | Every ideal path has a defined degradation: native tool calls fall back to text parsing; a missing MCP `inputSchema` defaults to `{"type":"object"}`; pursuit completion is deferred while checklist work remains. The system never silently relies on the happy path. | [Turns and rounds](turns-and-rounds.md), [MCP servers](mcp.md), [Pursuits](pursuits.md) |
| **Control plane vs domain** | The harness owns steering (mode, pursuit, retry, loop); providers and tools own I/O. `SubagentTool` lives in the agent crate because spawning a sub-agent is steering, not a domain action. | [Harness architecture](harness.md), [Sub-agents](subagents.md) |

## The canon, in reading order

A new contributor can read these top to bottom and end up with a complete
model of one agent turn.

1. [Harness architecture](harness.md) — the control plane around every
   provider call: turn execution, the two capability surfaces, the permission
   broker, the durable session, context compaction, and safety bounds. Start
   here.
2. [Turns and rounds](turns-and-rounds.md) — the two-layer execution model:
   a turn as the user-perceived unit, a round as the ReAct loop iteration
   inside it, and which concerns attach to each layer. Then the lifecycle
   inside one round: declaration, gating, execution, and how outcomes
   re-enter the conversation. This is the structural map the rest of the
   canon is built on.
3. [Prompt and message assembly](prompt-assembly.md) — the integrating view of
   what the model actually reads each turn: the system message recomposed from
   live state, the user channel carrying genuine input alongside
   harness-injected steering, tools declared through the native schema surface
   rather than described in prose, and the provenance discipline that makes the
   whole assembly auditable. Read after the structural map to see how every
   mechanism below feeds the model's context.
4. [Session persistence](session-persistence.md) — the durable local scene:
   model window, archived transcript, projection metadata, task and pursuit
   state, and the resume contract that keeps pruning and compaction from being
   rediscovered after restart.
5. [Model context](model-context.md) — the provider-facing request view:
   rebuilt system prompt, model-visible messages, tool schemas, assistant tool
   calls, tool results, and provider-specific serialization.
6. [Pursuits](pursuits.md) — durable per-session objectives driven by the
   `/pursue` stop-gate (within-turn continuation until the condition is met)
   and the `/repeat` cron scheduler. How the agent keeps working toward an
   objective across rounds and restarts.
7. [Sub-agents](subagents.md) — the `subagent` tool's isolated child agent.
   The reference for isolation: what is shared (the provider), what is fresh
   (history, pursuits, plan state), how events stream back through one
   pipeline, and how a profile admits tools by capability axis.
8. [MCP servers](mcp.md) — local stdio MCP servers as dynamically discovered
   tools. The reference for failure isolation and for how an extension surface
   reuses the same `Tool` trait and execution path as built-ins.
9. [User questions](user-questions.md) — the `ask_user` tool that blocks a turn
   to resolve ambiguity. The reference for the oneshot-channel blocking
   pattern the permission broker also uses.
10. [Skills](skills.md) — on-demand domain expertise: the two-channel model
   (catalog in the system prompt, body on demand), the source/priority
   cascade, and explicit versus implicit invocation. The reference for the
   extension surface that adds instructions rather than tools.
11. [Lifecycle hooks](hooks.md) — user-configured actions that fire on the
   agent's lifecycle events (tool call, turn end, session start, compaction).
   One event axis with capability implied by the event; the reference for
   the extension surface that adds practice (format, CI gates, context
   injection) without touching the core loop.

The harness's [context projection](harness.md#context-projection) section has two
deep-dive references, read as a pair:

12. [Context pruning](context-pruning.md) — the cheap, implicit first layer:
    clearing stale tool-result bodies while preserving the `tool_call_id`
    chain, gated at ~65% of the window, surfaced only as a `debug` trace.
13. [Context compaction](context-compaction.md) — the heavier second layer:
    summarizing older complete turns into a durable checkpoint at ~85%, with
    a model-written anchored summary, deterministic fallback, and the visible
    `Compacted` notice.

## How a turn flows through the canon

A single agent turn touches almost every page. Tracing it is the fastest way
to see how the canon fits together:

```text
user message
  └─ [Harness] execute_turn
       ├─ [Session]   use the durable model window and projection metadata
       ├─ [Prompt]    rebuild system prompt and request-scoped model context
       └─ [Pursuits]  active pursuit injected into the prompt
       └─ [Hooks]     UserPromptSubmit: deny? / prepend context
       └─ [Provider] stream tokens; reconstruct native tool-call deltas
            └─ fallback? [Tool rounds] parse tool call from text
       └─ per tool call:
            ├─ [Hooks] PreToolUse gate (matcher?) ── deny? → blocked
            ├─ [Harness] permission broker (Write tools only)
            ├─ [Sub-agents] if call is `subagent`: spawn isolated child,
            │              stream SubTaskEvent back through the same pipeline
            ├─ [MCP]       if call is `mcp__*`: JSON-RPC over stdio
            └─ [User questions] if call is `ask_user`: block on oneshot
       └─ [Hooks] PostToolUse | PostToolUseFailure: inject context?
       └─ completion marker? [Pursuits] finalize on completion signal
       └─ next tool round, or stop on final message / safety bound
            └─ [Hooks] Stop gate composes with /pursue: deny? → another round
```

Every arrow is documented in one of the canon pages above.

## Decision history

For the frozen rationale behind specific choices (why the progress panel, why
the strict layering, why planning became a subagent and was later removed),
see the [Architecture Decision Records](../../adr/). ADRs link back into this
section for background; this section links to ADRs for the decision trail.

## Adjacent layers

The canon describes the agent. Two other concerns live alongside it in
`docs/explanation/` and are intentionally outside this section:

- **Provider protocol** — [Chat API primitives](../chat-api-primitives.md),
  [Request flow](../request-flow.md),
  [Provider capabilities](../provider-capabilities.md),
  [Guided decoding](../guided-decoding.md): the protocol contract that shapes
  the agent, its wire-level form, and constrained decoding. These belong to
  the model-serving layer, not the agent.
- **Terminal UI** — [Terminal UI](../tui.md): how the semantic document model
  renders the canon's events to the screen.
