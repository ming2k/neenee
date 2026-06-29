# neenee-agent

The orchestration layer between the pure domain (`neenee-core`) and the
application services (`neenee-store`) on one side, and the frontends on the
other.

## What lives here

- **The `Agent` struct** (`agent.rs`) — holds the provider, tool set, mode,
  pursuit, and skill registry; runs the streaming ReAct loop.
- **The turn/round loop** — tool-call parsing, permission brokering, context
  pressure (summarisation / context projection per ADR-0029), and the steering
  inbox.
- **Skills & MCP** — skill registry/loading and the Model Context Protocol
  client integration.
- **Catalog & envoy** — model/channel resolution and sub-agent ("envoy")
  profiles.

This crate is I/O-free at the domain level but drives `neenee-store` and
`neenee-providers`. Frontends (`neenee-code`, `neenee-quant`) sit above it via
`neenee-server`'s transport.
