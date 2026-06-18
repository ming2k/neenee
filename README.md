# neenee

<p align="center">
  <b>A Rust-based interactive AI coding agent with a semantic TUI, native and fallback tool use, on-demand skills, and a bounded autonomous goal harness.</b>
</p>

<p align="center">
  <!-- 如已配置 CI / crates.io，可取消注释以下徽章 -->
  <!-- <a href="#"><img src="https://img.shields.io/badge/rust-2021%2B-orange?logo=rust" alt="Rust 2021+"></a> -->
  <!-- <a href="#"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a> -->
  <!-- <a href="#"><img src="https://img.shields.io/badge/crates.io-v0.1.0-cyan" alt="Crates.io"></a> -->
</p>

---

## Table of Contents

- [Features](#features)
- [Architecture](#architecture)
- [Requirements](#requirements)
- [Installation](#installation)
- [Getting Started](#getting-started)
- [Commands](#commands)
- [Configuration](#configuration)
  - [API Keys](#api-keys)
  - [MCP Servers](#mcp-servers)
  - [Sessions](#sessions)
- [Customizing](#customizing)
  - [LLM Providers](#llm-providers)
  - [Tools](#tools)
  - [Skills](#skills)
  - [Custom Commands](#custom-commands)
- [Contributing](#contributing)
- [License](#license)

---

## Features

- **Semantic TUI** — Ratatui-based interface with semantic document selection and live harness status.
- **Native & Fallback Tool Use** — Full tool-capable ReAct path; OpenAI-compatible tool-call delta reconstruction.
- **Bounded Autonomy** — Goal checklist with autonomous loops, cancellable retries, and side-effect-aware replay protection.
- **Context-Aware Controls** — `Ctrl+C` intelligently copies, interrupts, closes modals, or exits; `Ctrl+T` toggles tool expansion.
- **MCP Support** — Discover and use local stdio MCP servers alongside native tools.
- **Durable Sessions** — Atomic session persistence with compaction, resume, and fork support.

---

## Architecture

| Crate | Responsibility |
|-------|----------------|
| `neenee-core` | Agent harness, providers, tools, goals, and skills. |
| `neenee-tui` | Ratatui UI with semantic document selection and live harness status. |
| `neenee` | Provider wiring, slash commands, cancellation, and autonomous loops. |

Provider streaming stays inside the harness, including reconstruction of OpenAI-compatible tool-call deltas before permission checks and execution.

---

## Requirements

- [Rust](https://rustup.rs/) (Edition 2021+)

---

## Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/neenee.git
cd neenee

# Build and run in development mode
cargo run

# Or build the release binary
cargo build --release
# The binary will be at ./target/release/neenee
```

---

## Getting Started

```bash
# Start a fresh session
cargo run

# Resume the most recent session
cargo run -- resume

# Resume a specific session by ID
cargo run -- resume <id>
```

---

## Commands

### Harness Commands

| Command | Description |
|---------|-------------|
| `/mode build` | Switch to Build mode. |
| `/mcp` | Inspect MCP connection state. |
| `/permissions` | View or manage tool permissions. |
| `/permissions clear` | Revoke all cached permission rules. |
| `/session status` | Show current session status. |
| `/resume` | Resume the most recent cached conversation. |
| `/session fork` | Fork the current session. |
| `/session list` | List all available sessions. |
| `/session open <short-id>` | Open a specific session. |
| `/compact` | Manually compact older turns to save context. |
| `/goal <description>` | Create and start a new goal. |
| `/goal done` | Mark the current goal as completed. |
| `/loop 8` | Start an autonomous loop with up to 8 turns. |
| `/loop resume` | Resume the last autonomous loop. |
| `/loop status` | Show current loop status. |
| `/loop stop` | Stop the running autonomous loop. |

### TUI Controls

| Shortcut | Action |
|----------|--------|
| `Ctrl+T` | Expand / collapse full tool arguments and output. |
| `Ctrl+M` | Open the model selection modal. |
| `Ctrl+C` | Context-aware: copy selection → interrupt response → close modal → clear input → exit (double press). |
| `/exit` or `q` (empty prompt) | Quit the program. |

### Key Behaviors

- **Goal Checklist** — Structured progress is shown as `done/total`. An autonomous loop cannot accept its completion marker while checklist work remains.
- **Tool Rounds** — Every normal turn uses the full tool-capable ReAct path; the harness stops a turn after **32 tool rounds** or **3 identical consecutive tool calls**.
- **Retries** — Transient provider rate limits, overloads, timeouts, and connection failures use cancellable bounded retries with visible countdown. Retries are disabled as soon as a tool call occurs to prevent side-effect replay.
- **Permissions** — Write-capable tools pause for a blocking decision: **Allow once**, **Always allow**, or **Reject**. Run `/permissions clear` to revoke cached rules.

---

## Configuration

### API Keys

Open the solution modal with `/models` or `Ctrl+M`. Presets include:

| Preset | Notes |
|--------|-------|
| **Kimi Code** | Uses the official `kimi-for-coding` model ID. Defaults to the approved OpenCode-compatible identity `opencode/1.17.4`. |
| **OpenAI** | Standard OpenAI-compatible endpoint. |
| **Gemini** | Google's Gemini models. |
| **Kimi Open Platform** | Kimi API platform endpoint. |
| **Custom Relay** | Prompts for a full OpenAI-compatible endpoint, model ID, and API key. |

Each preset shows whether a usable key is configured:
- `✓ ready` — key present
- `✗ no key` — key missing

Values are saved to `~/.config/neenee/config.toml`. Environment variables take precedence:

| Variable | Description |
|----------|-------------|
| `KIMI_CODE_API_KEY` | API key for Kimi Code. |
| `KIMI_CODE_USER_AGENT` | Custom user agent for Kimi Code. |
| `CUSTOM_BASE_URL` | Base URL for the Custom relay preset. |
| `CUSTOM_MODEL` | Model ID for the Custom relay preset. |

### MCP Servers

Local stdio MCP servers are discovered from `~/.config/neenee/config.toml`:

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
read_only = false
```

- On startup, their tools are registered as `mcp__<server>__<tool>`.
- A failed MCP server is isolated and does not prevent neenee from starting.
- MCP servers default to **write-capable**; they are blocked in **Plan mode** unless explicitly configured with `read_only = true`.

### Sessions

| Path | Purpose |
|------|---------|
| `~/.config/neenee/session.json` | Current conversation and loop checkpoint (atomic writes). |
| `~/.config/neenee/sessions/` | Cached historical sessions. |

- **Fresh session** — Starting neenee without arguments creates a new empty session while keeping the last selected provider and model.
- **Resume** — `/resume` restores the most recent conversation; `/resume <short-id>` targets a specific one; `/session list` shows all available.
- **Compaction** — Older complete turns are compacted automatically when active context exceeds the configured character budget. Full history remains archived in `session.json`. `/compact` triggers this manually.
- **Context Overflow** — Retried once only before any tool activity has occurred.

---

## Customizing

### LLM Providers

Edit `crates/neenee-core/src/providers.rs` to add new LLM backends (e.g., Anthropic, Ollama).

### Tools

Add new tools in `crates/neenee-core/src/tools.rs` by implementing the `Tool` trait.

### Skills

Add project-specific skills to `.neenee/skills/*.md` or user-wide skills to `~/.neenee/skills/*.md`. Metadata is indexed at startup and bodies are loaded on demand through `use_skill`.

### Custom Commands

Add reusable slash commands to `.neenee/commands/*.md` or `~/.neenee/commands/*.md`. Project commands override user commands with the same name and appear in slash autocomplete.

**Syntax example:**

```markdown
---
description: Review the current changes
---
Review $ARGUMENTS against $1 and report correctness risks first.
```

- `$ARGUMENTS` — expands to the raw argument string.
- `$1` … `$9` — expand to parsed positional arguments.

Commands run through the normal permissions, cancellation, retry, and durable session harness. Shell interpolation in command markdown is intentionally **not** executed outside the tool pipeline.

---

## Contributing

Contributions are welcome! Please feel free to open an issue or submit a pull request.

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Commit your changes (`git commit -am 'Add some feature'`)
4. Push to the branch (`git push origin feature/my-feature`)
5. Open a Pull Request

---

## License

This project is dual-licensed under either:

- **MIT License**
- **Apache License, Version 2.0**

at your option.

