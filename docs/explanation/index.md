# Explanation

Conceptual background and design rationale.

## Agent design

The design canon for neenee's agent — how a turn is steered, gated, isolated,
made durable, and kept honest. The seven pages share a set of recurring themes
(capability gating, isolation boundaries, durable vs ephemeral state,
streaming, fallback, control-plane separation) that the section index lays out
before the individual docs.

| Page | Purpose |
|------|--------|
| [Agent design](agent-design/index.md) | Section index: the recurring design themes, a suggested reading order, and how a turn flows through the canon |
| [Harness architecture](agent-design/harness.md) | Control plane around provider calls, goal state, autonomous loop, safety bounds |
| [Turns and rounds](agent-design/turns-and-rounds.md) | The two-layer execution model: a turn as the user-perceived unit, a round as the ReAct loop iteration inside it, and which concerns attach to each layer |
| [Goals](agent-design/goals.md) | Durable per-session objectives: status machine, checklist, token budget, and completion deferral |
| [Sub-agents](agent-design/subagents.md) | The `task` tool's read-only child agent: isolation model, event streaming, and the TUI zoom view |
| [MCP servers](agent-design/mcp.md) | Local stdio MCP server discovery, the `mcp__<server>__<tool>` wrapper, failure isolation, and Plan-mode gating |
| [Plan mode](agent-design/plan-mode.md) | Read-only planning surface, autonomous `plan_enter`/`plan_exit`, and the plan-file write exemption |
| [User questions](agent-design/user-questions.md) | How the `ask_user` tool blocks the agent, renders a modal, and returns answers |
| [Tool rounds](agent-design/tool-rounds.md) | The round trip of a tool call as a design concept: declaration, gating, execution, and how outcomes re-enter the conversation |

## Provider protocol and UI

Layers adjacent to the agent: the wire-level contract with model servers, and
the terminal rendering surface.

| Page | Purpose |
|------|--------|
| [Terminal UI](tui.md) | How the TUI is built (full-screen app, semantic document model, live rendering) and why it is not terminal text |
| [Request flow](request-flow.md) | HTTP transaction shape, SSE streaming, and the ReAct loop's message evolution |
| [Provider capabilities](provider-capabilities.md) | Where tool calling and reasoning actually live across model weights, serving runtime, and client |
| [Guided decoding](guided-decoding.md) | Constrained decoding, FSM compilation, and chat templates — the layer that guarantees valid tool calls |
