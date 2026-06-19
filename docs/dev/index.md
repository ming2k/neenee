# Contributor docs

| Page | Purpose |
|------|---------|
| [Documentation governance](documentation/index.md) | Rules for organizing, writing, and reviewing docs |

## Architecture

- [Harness architecture](../explanation/harness.md) — control plane around provider calls, goal state, autonomous loop, safety bounds
- [Request flow](../explanation/request-flow.md) — HTTP transaction shape, SSE streaming, and the ReAct loop's message evolution
- [Provider capabilities](../explanation/provider-capabilities.md) — where tool calling and reasoning live across model weights, serving runtime, and client
- [Guided decoding](../explanation/guided-decoding.md) — constrained decoding, FSM compilation, and chat templates
- [Tool lifecycle](../explanation/tool-lifecycle.md) — end-to-end tool round trip: schema declaration, model interaction, dispatch, execution, and result handling
