# neenee

<p align="center">
  <b>A Rust-based AI coding agent with semantic TUI, tool use, on-demand skills, and bounded autonomous goals.</b>
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

- **Semantic TUI** — Ratatui-based interface with semantic selection and live status.
- **Native & Fallback Tool Use** — Full tool-capable ReAct path; OpenAI-compatible delta reconstruction.
- **Bounded Autonomy** — Goal checklists, autonomous loops, cancellable retries, and replay protection.
- **Context-Aware Controls** — `Ctrl+C` copies, interrupts, or exits contextually; `Ctrl+T` toggles tool expansion.
- **MCP Support** — Discover and use local stdio MCP servers alongside native tools.
- **Durable Sessions** — Atomic persistence with compaction, resume, and fork support.

---

## Architecture

Strictly-layered six-crate workspace (see ADR-0005 for the topology and renames):

| Crate | Responsibility |
|-------|----------------|
| `neenee-core` | Pure domain types: events, messages, tools, goals, skills config, capability traits. No I/O. |
| `neenee-providers` | Concrete LLM providers (Kimi, OpenAI-compatible, Gemini native, Mock) and the `build_provider_for_channel` factory. |
| `neenee-tools` | Concrete `Tool` implementations (bash, read/write/edit, glob, grep, web search, MCP loader, project init). |
| `neenee-store` | Local coding-agent persistence: event-sourced sessions, blob store, config, paths, advisory locks, embedding index. |
| `neenee-agent` | The `Agent` struct, turn orchestration, compaction, retries, model/channel catalog, skills, and `TaskTool`. |
| `neenee-cli` | The `neenee` binary: inlined Ratatui TUI, slash commands, cancellation, and autonomous loops. |

Dependency direction is strict: `core` ← {`providers`, `tools`, `store`} ← `agent` ← `cli`, with no reverse edges. Provider streaming and tool-call delta reconstruction stay inside the `agent` layer, before tool execution.

---

## Requirements

- [Rust](https://rustup.rs/) (Edition 2021+)

---

## Installation

```bash
git clone https://github.com/yourusername/neenee.git
cd neenee
cargo run
cargo build --release  # binary at ./target/release/neenee
```

---

## Getting Started

```bash
cargo run
cargo run -- resume
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
| `Ctrl+B` | Switch from input (Compose) to conversation stream (Browse). Press any key (typically `p`) to return. |
| `Ctrl+T` | Expand / collapse tool arguments and output. |
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
| **Kimi K2.7 Code** | Moonshot AI's strongest coding model via the official `api.moonshot.ai` endpoint. 256K context. |
| **OpenAI** | Standard OpenAI-compatible endpoint. |
| **Gemini 2.5 Flash** | Google's Gemini 2.5 Flash model. |
| **DeepSeek V4 Flash** | DeepSeek V4 Flash via the official `deepseek-v4-flash` model. |
| **DeepSeek V4 Pro** | DeepSeek V4 Pro via the official `deepseek-v4-pro` model. |
| **Qwen Plus** | Alibaba DashScope. |
| **GLM 4 Plus** | Zhipu AI. |

Each preset shows whether a usable key is configured:
- `✓ ready` — key present
- `✗ no key` — key missing

Values are saved to `~/.config/neenee/config.toml`. Environment variables take precedence:

| Variable | Description |
|----------|-------------|
| `MOONSHOT_API_KEY` | API key for Kimi K2.7 Code. |
| `GEMINI_API_KEY` / `GEMINI_MODEL` | API key / model override for Gemini (default `gemini-2.5-flash`). |
| `DEEPSEEK_API_KEY` | Shared API key for DeepSeek V4 Flash and Pro. |
| `DEEPSEEK_FLASH_MODEL` / `DEEPSEEK_PRO_MODEL` | Model override for DeepSeek V4 Flash / Pro. |

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
- **Compaction** — Context pressure is relieved in layers: old tool results are pruned (between tool rounds and before turns), and when size still exceeds the configured character budget older complete turns are replaced by an anchored LLM-generated summary (with a deterministic excerpt fallback). Full history remains archived in `session.json`. `/compact` triggers a summary manually.
- **Context Overflow** — Retried once only before any tool activity has occurred.

---

## Customizing

### LLM Providers

Edit `crates/neenee-providers/src/lib.rs` to add new LLM backends (e.g., Anthropic, Ollama). See [How to add a provider](docs/how-to/add-a-provider.md).

### Tools

Add new tools in `crates/neenee-tools/src/lib.rs` by implementing the `Tool` trait. See [How to add a tool](docs/how-to/add-a-tool.md).

### Skills

Skills are domain-specific instruction files that neenee can load on demand or automatically when mentioned.

#### Layout

Place skills in directories named `SKILL.md`:

```
.neenee/skills/<name>/SKILL.md
~/.neenee/skills/<name>/SKILL.md
```

Skill files use YAML frontmatter followed by Markdown content:

```markdown
---
name: rust-expert
description: "Use when writing or debugging Rust code"
short-description: "Rust help"
version: "1.0.0"
tags: [rust, cargo]
policy:
  allow_implicit_invocation: true
dependencies:
  tools:
    - type: mcp
      value: rust-analyzer
---

# Rust Expert

... guidelines, examples, checklists ...
```

#### Discovery sources (highest priority wins)

1. Project repo: `.neenee/skills/**/SKILL.md`, `.agents/skills/**/SKILL.md`, `.claude/skills/**/SKILL.md`, `.kimi-code/skills/**/SKILL.md`
2. User global: `~/.neenee/skills/**/SKILL.md`, `~/.agents/skills/**/SKILL.md`, `~/.claude/skills/**/SKILL.md`, `~/.kimi-code/skills/**/SKILL.md`
3. Extra local paths configured in `config.toml`
4. Remote skill repositories configured in `config.toml`

#### Configuration

Add a `[skills]` table to `~/.config/neenee/config.toml`:

```toml
[skills]
paths = ["~/.my-skills"]
urls = ["https://example.com/skills"]
disabled = ["old-skill"]
bundled = true
```

Remote repositories must expose an `index.json`:

```json
{
  "skills": [
    { "name": "my-skill", "files": ["SKILL.md", "reference.md"] }
  ]
}
```

Remote files are cached under `~/.cache/neenee/skills/`.

#### Using skills

- `use_skill` tool — load a skill by name.
- `list_skills` tool — list all available skills.
- `reload_skills` tool — rescan local directories and refetch remote repos.
- Slash commands: `/skills list`, `/skills reload`, `/skill <name>`.

Skills with `policy.allow_implicit_invocation: true` are automatically injected into the context when you mention them by name, e.g. `rust-expert: review this` or `@rust-expert`.

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

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.

