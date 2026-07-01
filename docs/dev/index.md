# Contributor docs

| Page | Purpose |
|------|---------|
| [Release process](release.md) | Versioning, the pre-tag CI checklist, and the tag/publish workflow |
| [Documentation governance](documentation/index.md) | Rules for organizing, writing, and reviewing docs |
| [TUI component showcase](showcase.md) | Interactive playground for rendering and testing individual TUI modals in isolation |

## Architecture

- [Persistence and the XDG layout](../explanation/persistence.md) — why every persistent path flows through the central `Dirs` layer and the four-category split
- [Harness architecture](../explanation/agent-design/harness.md) — control plane, provider calls, pursuit state, autonomous loop
- [Request flow](../explanation/request-flow.md) — HTTP transactions, SSE streaming, ReAct loop
- [Provider capabilities](../explanation/provider-capabilities.md) — tool calling and reasoning across model weights, runtime, and client
- [Guided decoding](../explanation/guided-decoding.md) — constrained decoding, FSM compilation, chat templates
- [Tool rounds](../explanation/agent-design/rounds-and-turns.md) — tool call lifecycle: declaration, gating, execution, and re-entry

## Policy

- [ADR-0014: Unified XDG persistence architecture](../adr/0014-xdg-persistence-architecture.md) — new persistent locations must be added as methods on `Dirs`, classified by what the file *is*; no inline `dirs::home_dir().join(...)` for neenee-owned storage
