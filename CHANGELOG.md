# Changelog

All notable changes to **neenee** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **In-loop loop detection steers a stuck turn before the hard abort
  (ADR-0030).** A model that repeats the same or near-identical read-only
  actions (micro-adjusted `read_file` ranges, `grep` argument tweaks) no longer
  runs unchecked until the equality guard's hard abort — its arguments never
  compare equal, so it bypassed the guard entirely. The harness now fires the
  semantic review (`/review`'s `LoopingReview`) once per turn on a read-only
  round streak or a repeated-call count, and on a `Stuck` verdict injects an
  **anti-anchoring nudge** that names the loop, forbids re-reading, and demands
  a forward action — non-terminating, so the user keeps `Esc` and the opt-in
  `hard_stop_rounds` as the backstop. The new `steering` module is the one home
  for built-in nudges.
- **A constrained `Round` lifecycle hook (ADR-0030, partially supersedes
  ADR-0025).** A new `event = "Round"` hook fires once per tool round, carrying
  the read-only-round streak. Unlike other events it is **`Deny`-forbidden** —
  a round-count hook may inject context or observe, but cannot become a de-facto
  round cap (the ADR-0009 concern). The harness declares no built-in threshold
  on this axis; it only provides the trigger point users opt into.
- New `[agent] loop_review_enabled` config key (default `true`) toggles the
  in-loop review. Always off on sub-agents (no `/review` path, no recursion).

### Changed

- **Modals no longer erase the background.** Opening a centered modal used to
  fully occlude the transcript, input, hint bar, and activity bar. Every modal
  except the sessions picker now **dims** the live surface in place instead —
  the background stays visible for context while the modal reads as the focal
  layer. The dim brightness is tunable via the new `modal_dim_factor` theme
  field (default 0.5). The sessions picker keeps its full-takeover behavior
  (footer collapse + solid occlusion) since switching sessions is a context
  switch. This is driven by a single new `Modal::recess` policy
  (`None` / `Dim` / `Takeover`) that the footer-collapse flag and the
  per-frame paint both consult, replacing the old opaque `draw_dim_backdrop`
  fill.

## [0.2.0] - 2026-06-24

### Removed

- **The per-plan progress tracker is consolidated into the unified task list
  (ADR-0020, supersedes ADR-0007).** `update_plan_progress`, the
  `PlanProgress` / `PlanSection` / `PlanSectionStatus` types, the
  `PlanProgressUpdated` events, and the persisted `plan_progress` session field
  are removed — they duplicated the `todo` / `todo_update` task list, which is
  now the single source of truth. `plan_exit` now seeds one `TodoList` from the
  approved plan's `##` headings; `plan_enter` clears it; the
  model tracks steps with `todo` / `todo_update`. One list, one panel, one
  persisted field. Old sessions load with graceful degradation: the dropped
  field triggers at most a checksum warning, and stale `plan_progress_set`
  event-log lines are skipped, so previously persisted progress is simply not
  restored.

### Changed

- **Context compaction is now relative to the active model's context window
  (ADR-0019).** Compaction previously triggered on a single fixed character
  budget (`compaction_max_chars`, default ~30k tokens) regardless of model —
  so a 1M-token model was over-compacted at ~3% of its window and a 128k model
  was merely coincidental. Thresholds are now derived from the live model's
  context window (token-denominated): cheap tool-result pruning at 65%, a full
  summarizing compaction at 85%, compressed toward a 25% target, with a 32k
  fallback window for unknown/local models. The mid-turn prune threshold is
  re-seeded on every `/provider` switch so relief tracks the current model
  instead of the one active at startup. Pressure is estimated in tokens to
  match the window's unit; provider-reported `prompt_tokens` is a future
  enhancement that slots in without changing the threshold shape. See the
  [Configuration Reference](docs/reference/configuration.md#compaction).
  - Config: `compaction_max_chars` and `compaction_prune_protect_chars` are
    removed; a `[compaction]` table (`utilization`, `target_utilization`,
    `prune_utilization`, `fallback_window_tokens`) and
    `compaction_prune_protect_tokens` (default 6_000) replace them. Existing
    `config.toml` files keep parsing (the removed keys are ignored).

- **The base system prompt now directs the agent to be concise and direct.**
  `build_system_prompt` previously stated only the agent's identity and current
  mode; it now also sets explicit output norms — answer with the minimum
  needed, skip preamble and recaps, no unsolicited explanations or code
  comments, never commit unless asked, take the reasonable action with ordinary
  tools instead of asking permission, prefer dedicated file tools over shell
  pipelines, and verify with the project's build/tests/linter when correctness
  is implied. This brings neenee's default conversational behavior in line with
  the conciseness baseline that other coding agents (codex, opencode,
  claude-code) ship in their base prompts. No mechanism change; only the
  assembled system message wording.

- **Session review replaces the round-counting stall detector (ADR-0016).**
  The read-only "stall detector" (a reflection nudge at 8 read-only rounds and
  a hard abort at 14) is removed — it was an arbitrary cap ADR-0009 had
  rejected, and "no write fired" is a poor proxy for "stuck" (it mis-flagged
  legitimate exploration, especially read-only research sub-agents). In its
  place, after `review_start_round` (default 64) tool rounds and every
  `review_interval_rounds` (default 16) thereafter, the harness spawns a
  bounded read-only diagnostic sub-agent that reads the live transcript and
  returns a verdict per pluggable review dimension (`LoopingReview` first).
  Review surfaces an alert (and a one-shot reflection nudge on a `Stuck`
  verdict) but **never aborts the turn**; the only execution cap is an opt-in
  `hard_stop_rounds` (default 0 = off). Sub-agents run with review disabled.
  - Config: `[agent] stall_threshold` → `[agent.review]` (`review_start_round`,
    `review_interval_rounds`, `hard_stop_rounds`).
  - Slash command: `/stall-threshold` → `/review` (`/review off`,
    `/review N [M]`, `/review default`).
  - Events: `StallWarning` → `SessionReview { alert }`.

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

[Unreleased]: https://github.com/ming2k/neenee/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ming2k/neenee/releases/tag/v0.2.0
[0.1.0]: https://github.com/ming2k/neenee/releases/tag/v0.1.0
[0.0.1]: https://github.com/ming2k/neenee/releases/tag/v0.0.1
