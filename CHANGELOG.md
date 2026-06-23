# Changelog

All notable changes to **neenee** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-06-24

### Added

- **`/pursue <condition>`** — a Claude-Code-style **stop-gate**. Setting a
  pursuit persists the condition and drives a single agent turn that refuses
  to end until the model signals completion (`[NEENEE_PURSUIT_COMPLETE]`), a
  50-round safety cap is hit, or you interrupt (`/pursue stop` / `Esc`). The
  gate re-injects the condition on each stop attempt, so the pursuit is
  within-turn continuation. Subcommands: `/pursue` (re-arm), `status`, `edit`,
  `done`, `stop`, `clear`.
- **`/repeat <cron> <prompt>`** — a durable **cron scheduler**. A real
  five-field cron expression engine fires the prompt as a normal turn on a
  clock. Jobs persist in `repeat.db` (survive restarts), auto-expire after 30
  days, and the first run fires immediately. `/repeat list`, `/repeat cancel
  <id>`, `/repeat help`.

### Removed

- **`/goal` and `/loop`.** Replaced by `/pursue` (condition-driven stop-gate)
  and `/repeat` (clock-driven cron). `/loop resume` has no equivalent — a
  pursuit is a single turn. Migrate: `/goal <x>` + `/loop` → `/pursue <x>`.
- **The goal checklist primitive** (`goal_checklist` tool, checklist gating,
  completion-defer). Completion is now a single boolean driven by the
  completion marker.
- **Legacy pre-XDG skill and command paths.** neenee no longer scans
  `~/.neenee/skills/` or `~/.neenee/commands/`. Move their contents to the
  XDG locations to keep them loaded:
  ```bash
  mv ~/.neenee/skills/*   $XDG_DATA_HOME/neenee/skills/   2>/dev/null || true
  mv ~/.neenee/commands/* $XDG_DATA_HOME/neenee/commands/ 2>/dev/null || true
  rmdir ~/.neenee/skills ~/.neenee/commands ~/.neenee     2>/dev/null || true
  ```
- **`~/.kimi-code/skills/` external skill directory.** Only `~/.agents/skills/`
  and `~/.claude/skills/` are read as external application conventions now
  (both user-global and project-local). Move any kimi-code skills into one of
  the remaining external directories or the neenee XDG skill directory.

### Fixed

- **Skill discovery priority now overrides as documented.** A higher-priority
  source (project-local, then configured paths, then user-global, then remote,
  then bundled) now correctly overrides a lower-priority source that defines a
  skill with the same name. Previously the first source scanned won, which
  inverted the intended priority.

## [0.0.1] - 2026-06-24

First usable release. neenee is now a working AI coding agent with a semantic
TUI, tool use, on-demand skills, plan mode, and durable sessions.

### Added

- **Semantic TUI** built on Ratatui: live status, expandable tool steps,
  streaming bash output, structured diffs, per-step detail overlays, sticky
  headers, and a persistent right-side sidebar for plans and goal state.
- **Tool use** via a full ReAct loop with native and fallback tool-calling;
  bundled tools include bash, file read/write/edit, grep, glob, web search,
  and MCP server integration.
- **Autonomous goals**: set an objective with `/goal`, run `/loop`, and let
  the agent work iteratively with a checklist and bounded autonomy.
- **Plan mode**: read-only analysis and planning that does not touch the
  codebase, plus `/plan` and `/verify` slash commands and a plan preview
  modal with a sticky progress panel and stale-plan detection.
- **Durable sessions**: atomic on-disk persistence with context compaction,
  session resume and fork, a sessions picker, and `/export` to Markdown.
- **Skills system**: domain-specific instructions loaded on demand or
  automatically by mention, stored under XDG paths with compile-time-embedded
  bundled skills.
- **Model and provider management**: provider/model picker (`Ctrl+M`),
  split provider and model registries, provider timeouts, and persistent
  per-session permissions with labeled permission prompts.
- **Sub-agents** with tool-admission profiles driven by a `ToolAccess` tier
  split, and an inline sub-agent view.
- **Reliability aids**: stalled-agent detection with a configurable verify
  hard nudge (`/stall-threshold`, `/verify-nudge`), plus an uncapped agentic
  loop anchored to a single breathing indicator.
- **Observability**: opt-in file-based tracing across the harness.
- **Slash commands**: `/goal`, `/loop`, `/compact`, `/plan`, `/verify`,
  `/session list`, `/export`, `/mcp`, `/stall-threshold`, and `/verify-nudge`.

### Changed

- Adopted a strict six-crate workspace topology
  (`neenee-core` ← `{neenee-providers, neenee-tools, neenee-store}` ←
  `neenee-agent` ← `neenee-cli`) with typed errors and a unified agent loop.
- Standardized on MIT-only licensing.

[Unreleased]: https://github.com/ming2k/neenee/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ming2k/neenee/releases/tag/v0.1.0
[0.0.1]: https://github.com/ming2k/neenee/releases/tag/v0.0.1
