# Explanation

Conceptual background and design rationale.

## Storage and persistence

How neenee decides where each file lives, why it follows the XDG Base
Directory Specification, and what is safe to delete.

| Page | Purpose |
|------|--------|
| [Persistence and the XDG layout](persistence.md) | The four-category model (config / data / state / cache), override precedence, and what each category means operationally |

## Agent design

The design canon for neenee's agent — how a turn is steered, gated, isolated,
made durable, and kept honest. The pages share a set of recurring themes
(capability gating, isolation boundaries, durable vs ephemeral state,
streaming, fallback, control-plane separation) that the section index lays out
before the individual docs.

| Page | Purpose |
|------|--------|
| [Agent design](agent-design/index.md) | Section index: the recurring design themes, a suggested reading order, and how a turn flows through the canon |
| [Harness architecture](agent-design/harness.md) | Control plane around provider calls, pursuit state, autonomous loop, safety bounds |
| [Turns and rounds](agent-design/turns-and-rounds.md) | The two-layer execution model (turn vs round) and the lifecycle inside one round: declaration, gating, execution, and how outcomes re-enter the conversation |
| [Pursuits](agent-design/pursuits.md) | The `/pursue` stop-gate (within-turn condition-driven pursuit) and the `/repeat` cron scheduler |
| [Sub-agents](agent-design/subagents.md) | The `subagent` tool's read-only child agent: isolation model, event streaming, and the TUI zoom view |
| [MCP servers](agent-design/mcp.md) | Local stdio MCP server discovery, the `mcp__<server>__<tool>` wrapper, failure isolation, and access-tier gating |
| [User questions](agent-design/user-questions.md) | How the `ask_user` tool blocks the agent, renders a modal, and returns answers |
| [Skills](agent-design/skills.md) | On-demand domain expertise: the catalog/body two-channel model, the source/priority cascade, and explicit versus implicit invocation |
| [Lifecycle hooks](agent-design/hooks.md) | User-configured actions on the agent's lifecycle events (PreToolUse, Stop, SessionStart, PreCompact…): one event axis with capability implied by the event |


## Provider protocol and UI

Layers adjacent to the agent: the chat API primitives that shape it, the
wire-level contract with model servers, and the terminal rendering surface.

| Page | Purpose |
|------|--------|
| [Chat API primitives](chat-api-primitives.md) | The three protocol primitives — role authority, stateless memory, function calling — that shape the agent |
| [Terminal UI](tui.md) | How the TUI is built (full-screen app, semantic document model, live rendering) and why it is not terminal text |
| [Request flow](request-flow.md) | HTTP transaction shape, SSE streaming, and the ReAct loop's message evolution |
| [Provider capabilities](provider-capabilities.md) | Where tool calling and reasoning actually live across model weights, serving runtime, and client |
| [Guided decoding](guided-decoding.md) | Constrained decoding, FSM compilation, and chat templates — the layer that guarantees valid tool calls |
