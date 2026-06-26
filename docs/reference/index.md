# Reference

Lookup-oriented documentation — tables, lists, and exact values.

## Tools and providers

- [Built-in tools](tools/) — tool catalog, access tiers, capability axes, and
  per-tool parameter schemas (one page per tool category)
- [Providers](providers.md) — capability matrix, endpoint and env var catalog

## Commands

- [Slash commands](commands.md) — built-in commands, subcommands, custom commands

## Configuration

- [Configuration](configuration.md) — every `config.toml` key with its default

## Files and persistence

- [Paths](paths.md) — every file neenee reads or writes, by XDG category,
  with override precedence and cleanup quick reference

## TUI

- [TUI overview](tui/) — component map, file responsibilities
- [Frame layout](tui/layout.md) — vertical chunks, chrome hiding, measurements
- [Color palette](tui/theme.md) — all theme tokens with RGB values
- [Half-block characters](tui/half-block-chars.md) — `╻╹▀▄┃` Unicode reference

### Components

| Component | File |
|-----------|------|
| User message | [user-message.md](tui/user-message.md) |
| Input box | [input-box.md](tui/input-box.md) |
| Assistant text | [assistant-text.md](tui/assistant-text.md) |
| Code block | [code-block.md](tui/code-block.md) |
| Expandable step | [expandable-step.md](tui/expandable-step.md) |
| Tool step | [tool-step.md](tui/tool-step.md) |
| Thinking step | [thinking-step.md](tui/thinking-step.md) |
| Step state machine | [step-state.md](tui/step-state.md) |
| Sub-agent view | [subagent-view.md](tui/subagent-view.md) |
| Activity bar | [status-bar.md](tui/status-bar.md) |
| Hint bar | [hint-line.md](tui/hint-line.md) |
| Modals | [modals.md](tui/modals.md) |
