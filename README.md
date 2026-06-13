# neenee

A Rust-based interactive AI coding agent with a semantic TUI, native and
fallback tool use, on-demand skills, and a bounded autonomous goal harness.

Provider streaming stays inside the harness, including reconstruction of
OpenAI-compatible tool-call deltas before permission checks and execution.

## Architecture

- **neenee-core**: Agent harness, providers, tools, goals, and skills.
- **neenee-tui**: Ratatui UI with semantic document selection and live harness status.
- **neenee-cli**: Provider wiring, slash commands, cancellation, and autonomous loops.

## Requirements

- Rust (Edition 2021+)

## Getting Started

```bash
cargo run --bin neenee-cli
```

Useful harness commands:

```text
/mode build
/mcp
/permissions
/session status
/resume
/session fork
/session list
/session open <short-id>
/compact
/goal implement and verify the requested feature
/loop 8
/loop resume
/loop status
/loop stop
/goal done
```

The header keeps the provider/model, Build or Plan mode, loop progress, current
goal, and semantic activity such as searching, editing, or running commands
visible. Structured goal checklist progress is shown as `done/total`, and an
autonomous loop cannot accept its completion marker while checklist work
remains. Tool calls and results are merged into compact semantic steps;
`Ctrl+T` expands or collapses their full arguments and output. Every normal
turn uses the full tool-capable ReAct path;
the harness stops a turn after 32 tool rounds or 3 identical consecutive tool
calls.

Transient provider rate limits, overloads, timeouts, and connection failures
use cancellable bounded retries with visible countdown status. Retries are
disabled as soon as a tool call occurs to prevent side-effect replay.

Write-capable tools pause for a blocking permission decision. Choose Allow
once, confirm Always allow for the current process, or Reject. Run
`/permissions clear` to revoke cached rules.

`Ctrl+C` is context-aware: it copies an active selection, interrupts a running
response, closes a modal, clears the input line, and — pressed twice on an
empty prompt — exits the program. `/exit` and `q` on an empty prompt also quit.

## API keys

Open the solution modal with `/models` or `Ctrl+M`. Presets include Kimi Code,
OpenAI, Gemini, the Kimi Open Platform, and other supported providers. Kimi
Code uses its official coding endpoint and the fixed `kimi-for-coding` model
ID, which the service maps to its latest coding model. Display names such as
K2.7 are not sent as the request model ID.

Each preset shows whether a usable key is configured (`✓ ready` / `✗ no key`).
The Custom relay solution prompts for a full OpenAI-compatible chat
completions endpoint, model ID, and API key. Values are saved to
`~/.config/neenee/config.toml`. Environment variables such as
`KIMI_CODE_API_KEY`, `KIMI_CODE_USER_AGENT`, `CUSTOM_BASE_URL`, and
`CUSTOM_MODEL` take precedence. The Kimi Code preset defaults to the approved
OpenCode-compatible identity `opencode/1.17.4`; override it when the approved
identity changes.

## Customizing

### LLM Providers
Edit `crates/neenee-core/src/providers.rs` to add new LLM backends (e.g., Anthropic, Ollama).

### Tools
Add new tools in `crates/neenee-core/src/tools.rs` by implementing the `Tool` trait.

### Skills

Add project skills to `.neenee/skills/*.md` or user skills to
`~/.neenee/skills/*.md`. Metadata is indexed at startup and bodies are loaded
on demand through `use_skill`.

### Custom Commands

Add reusable slash commands to `.neenee/commands/*.md` or
`~/.neenee/commands/*.md`. Project commands override user commands with the
same name and appear in slash autocomplete:

```markdown
---
description: Review the current changes
---
Review $ARGUMENTS against $1 and report correctness risks first.
```

`$ARGUMENTS` expands to the raw argument string and `$1` through `$9` expand
to parsed positional arguments. Commands run through the normal permissions,
cancellation, retry, and durable session harness. Shell interpolation in
command markdown is intentionally not executed outside the tool pipeline.

### MCP

Local stdio MCP servers are discovered from the normal config file:

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
read_only = false
```

On startup their tools are registered as
`mcp__<server>__<tool>`. Run `/mcp` to inspect connection state. A failed MCP
server is isolated and does not prevent neenee from starting.
MCP servers default to write-capable and are blocked in Plan mode unless the
server is explicitly configured with `read_only = true`.

### Sessions

The current conversation and loop checkpoint are written atomically to
`~/.config/neenee/session.json`, with cached sessions under
`~/.config/neenee/sessions/`. Starting neenee creates a fresh empty session
while keeping the last selected provider and model. Use `/resume` to restore
the most recent cached conversation, `/resume <short-id>` for a specific one,
or `/session list` to inspect available sessions.

Older complete turns are compacted automatically when active context exceeds
the configured character budget. Full history remains archived in
`session.json`; `/compact` performs the same operation manually. Context
overflow is retried once only before any tool activity has occurred.
