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

| Crate | Responsibility |
|-------|----------------|
| `neenee-core` | Agent harness, providers, tools, goals, and skills. |
| `neenee-tui` | Ratatui UI with semantic selection and live status. |
| `neenee` | Provider wiring, slash commands, cancellation, and autonomous loops. |

Provider streaming and tool-call delta reconstruction stay inside the harness, before execution.

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
- **Compaction** — Context pressure is relieved in layers: old tool results are pruned (between tool rounds and before turns), and when size still exceeds the configured character budget older complete turns are replaced by an anchored LLM-generated summary (with a deterministic excerpt fallback). Full history remains archived in `session.json`. `/compact` triggers a summary manually.
- **Context Overflow** — Retried once only before any tool activity has occurred.

---

## Customizing

### LLM Providers

Edit `crates/neenee-core/src/providers.rs` to add new LLM backends (e.g., Anthropic, Ollama).

### Tools

Add new tools in `crates/neenee-core/src/tools.rs` by implementing the `Tool` trait.

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

- **MIT License**
- **Apache License, Version 2.0**

at your option.

