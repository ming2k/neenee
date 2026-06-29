# neenee-code

The interactive coding-agent binary: a terminal UI for running the neenee
coding agent against a local checkout.

This is the primary user-facing crate. It wires together the foundation
(`neenee-core` + `neenee-store`), the LLM providers (`neenee-providers`), the
built-in tools (`neenee-tools`), the orchestration loop (`neenee-agent`), and
the session transport (`neenee-server`), then renders the interactive interface
via the in-house `neenee-tui` rendering engine (ADR-0038).

Run with:

```sh
cargo run -p neenee-code
```

See the top-level [`README.md`](../../README.md) for installation and usage, and
[`docs/`](../../docs/) for the architecture and how-tos.
