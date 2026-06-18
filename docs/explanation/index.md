# Explanation

Conceptual background and design rationale.

| Page | Purpose |
|------|---------|
| [Harness architecture](harness.md) | Control plane around provider calls, goal state, autonomous loop, safety bounds |
| [Request flow](request-flow.md) | HTTP transaction shape, SSE streaming, and the ReAct loop's message evolution |
| [Provider capabilities](provider-capabilities.md) | Where tool calling and reasoning actually live across model weights, serving runtime, and client |
| [Guided decoding](guided-decoding.md) | Constrained decoding, FSM compilation, and chat templates — the layer that guarantees valid tool calls |
| [Tool protocol](tool-protocol.md) | Wire-level protocol for declaring tools, transporting calls, and falling back to text |
