# neenee

English | [简体中文](./README.zh-CN.md)

A Rust-based AI coding agent with a semantic TUI, tool use, on-demand skills, and bounded autonomous pursuits.

<p align="center">
  <a href="#"><img src="https://img.shields.io/badge/rust-2021%2B-orange?logo=rust" alt="Rust 2021+"></a>
  <a href="#"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License"></a>
</p>

## Features

- **Semantic TUI** — Ratatui-based interface with live status, expandable tool steps, and structured diffs.
- **Tool Use** — Full ReAct loop with native and fallback tool-calling; bash, file I/O, grep, glob, web search, and MCP servers.
- **Autonomous Pursuits** — Set a pursuit with `/pursue <condition>` and the harness keeps the turn going (a stop-gate) until the condition is met. Schedule recurring prompts on a clock with `/repeat`.
- **Durable Sessions** — Atomic persistence with compaction, resume, and fork.
- **Skills** — Load domain-specific instructions on demand or automatically by mention.

## Quick Start

```bash
git clone https://github.com/ming2k/neenee.git
cd neenee
cargo run --release
```

On first launch, press `Ctrl+M` to pick a provider and enter your API key. Then just start typing.

## Key Bindings

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Tab` | Accept slash-command / `@path` completion |
| `Ctrl+M` | Open provider picker |
| `Ctrl+T` | Expand / collapse tool details |
| `Ctrl+B` | Toggle between input and conversation stream |
| `Ctrl+C` | Copy → interrupt → close modal → clear → quit |
| `Ctrl+V` | Paste from clipboard |

## Useful Commands

| Command | Description |
|---------|-------------|
| `/pursue <condition>` | Drive the agent until the condition is met (stop-gate) |
| `/repeat <cron> <prompt>` | Schedule a prompt on a cron expression |
| `/compact` | Compact context to free up space |
| `/session list` | Browse and resume past sessions |
| `/export` | Export conversation as Markdown |
| `/mcp` | Inspect MCP server connections |

## Architecture

Six-crate workspace with strict layering:

```
neenee-core  ←  {neenee-providers, neenee-tools, neenee-store}  ←  neenee-agent  ←  neenee-cli
```

See [docs/](docs/) for detailed architecture, guides, and reference.

## License

[MIT](LICENSE)
