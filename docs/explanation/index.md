# Explanation

Conceptual background and design rationale.

| Page | Purpose |
|------|---------|
| [Terminal UI](tui.md) | How the TUI is built (full-screen app, semantic document model, live rendering) and why it is not terminal text |
| [Harness architecture](harness.md) | Control plane around provider calls, goal state, autonomous loop, safety bounds |
| [Plan mode](plan-mode.md) | Read-only planning surface, autonomous `plan_enter`/`plan_exit`, and the plan-file write exemption |
| [Request flow](request-flow.md) | HTTP transaction shape, SSE streaming, and the ReAct loop's message evolution |
| [Provider capabilities](provider-capabilities.md) | Where tool calling and reasoning actually live across model weights, serving runtime, and client |
| [Guided decoding](guided-decoding.md) | Constrained decoding, FSM compilation, and chat templates — the layer that guarantees valid tool calls |
| [Tool protocol](tool-protocol.md) | Wire-level protocol for declaring tools, transporting calls, and falling back to text |
