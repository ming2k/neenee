# neenee

A Rust-based open-source AI coding agent inspired by OpenCode.

## Architecture

- **neenee-core**: Logic for Agents, Providers, and Tools.
- **neenee-daemon**: gRPC server (`tonic`) that manages sessions and executes agent tasks.
- **neenee-tui**: Terminal UI client that communicates with the daemon.

## Requirements

- Rust (Edition 2021+)
- `protoc` (Protobuf compiler)

## Getting Started

1.  **Start the Daemon**:
    ```bash
    cargo run -p neenee-daemon
    ```

2.  **Start the TUI**:
    ```bash
    cargo run -p neenee-tui
    ```

## Customizing

### LLM Providers
Edit `crates/neenee-core/src/providers.rs` to add new LLM backends (e.g., Anthropic, Ollama).

### Tools
Add new tools in `crates/neenee-core/src/tools.rs` by implementing the `Tool` trait.
