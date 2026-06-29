# neenee-core

Pure domain vocabulary for the neenee agent stack.

This crate is the **zero-I/O domain core** (ADR-0005): no `rusqlite`, no
filesystem, no network. It holds only the domain shapes and traits the rest of
the stack is built on:

- the [`Provider`] and [`Tool`] capability traits (in [`capability.rs`][cap]);
- conversation and tool-output types, the context-pressure model;
- pursuit / repeat / todo domain types, envoy profiles, skills/MCP config
  schemas;
- the wire events the harness and frontends exchange.

Frontends and sibling services depend on `neenee-core` for these traits and add
their own I/O layer on top. Anything that touches SQLite, the filesystem, or the
network belongs in [`neenee-store`](../neenee-store) instead.

See the architecture overview in [`docs/`](../../docs/) and ADR-0005 for the
zero-I/O boundary rationale.

[`Provider`]: src/capability.rs
[`Tool`]: src/capability.rs
[cap]: src/capability.rs
