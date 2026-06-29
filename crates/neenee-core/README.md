# neenee-core

Pure domain vocabulary for the coding-agent stack.

This crate is the **zero-I/O domain core** (ADR-0005): no `rusqlite`, no
filesystem, no network. It holds only the domain shapes and traits the rest of
the stack is built on:

- the [`Provider`](src/provider.rs) and [`Tool`](src/tool.rs) capability traits;
- conversation and tool-output types, the context-pressure model;
- pursuit / repeat / todo domain types, envoy profiles, skills/MCP config
  schemas;
- the wire events the harness and frontends exchange.

Frontends and sibling services depend on `neenee-core` for these traits and add
their own I/O layer on top. Anything that touches SQLite, the filesystem, or the
network belongs in [`neenee-store`](../neenee-store) instead.

See the architecture overview in [`docs/`](../../docs/) and ADR-0005 for the
zero-I/O boundary rationale.
