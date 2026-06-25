# Changelog

All notable changes to **neenee** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Inline code rendering.** Inline-code spans (`` `read_file` ``) in assistant
  prose, headings, list items, blockquotes, and reasoning traces are now styled
  on the same surface as fenced code blocks (`` `code_fg` `` on `` `code_bg` ``)
  instead of being flattened into the surrounding text with bare backticks. The
  markdown parser records each span's byte range on the prose block and the
  renderer paints the run (delimiters included) as a distinct chip, so the span
  reads as inline code at a glance. Copy and semantic selection are unchanged:
  the flattened text still carries the original backticks and the byte-addressable
  model is untouched, so selecting and copying an inline-code span yields the
  exact source.

## [Unreleased]

### Changed

- **Renamed the coding application: crate `neenee-cli` → `neenee-code`, binary
  `neenee` → `neenee-code`.** The workspace now has two domain applications
  (coding and quant), so neither carries the bare name. Every existing `neenee`
  invocation is now `neenee-code`. See [ADR-0035](docs/adr/0035-application-layer-split.md).

### Added

- **`neenee-quant` application crate** — the quantitative-trading application, a
  peer of `neenee-code` at the application layer. Depends on `neenee-agent` and
  layers on quant domain tools: `market_data`, `backtest`, `place_order`, and
  `list_positions`. The quant tools deliberately do not self-register, so a
  coding agent can never link `place_order` and a quant agent can never link
  `write_file` — domain isolation is enforced at assembly time. See
  [ADR-0035](docs/adr/0035-application-layer-split.md).

- **`QUANT` subagent profile** — a bounded subagent profile in `neenee-core`
  admitting read-only quant tools plus shared read-only inspection, while
  excluding live trading and all coding write/edit/exec tools.

## [0.6.1] - 2026-06-26

### Fixed

- **Non-streaming chat lost assistant content.** The OpenAI-compatible
  provider's `chat()` path fed the response through the tool-call echo filter
  but discarded `feed`'s return value, keeping only `finish()`'s output. Since
  `feed` emits ordinary prose as it classifies, every plain assistant response
  came back empty in the non-streaming path — silently breaking title
  generation, session summarization, and the non-streaming agent fallback. The
  streaming path was already correct. It now accumulates `feed`'s emission
  before resolving the held remainder with `finish`.

### Added

- **Provider wire-level integration tests.** A new `tests/wire.rs` in
  `neenee-providers` stands up a mock HTTP server (mockito) and drives the full
  request → HTTP → SSE-byte-reassembly → event-parse path for both the
  OpenAI-compatible and Anthropic `/messages` providers — covering header
  attachment, 5xx retry classification, keyless auth, text/reasoning/tool-call
  stream parsing, echo suppression, tool_use argument fragmenting, and in-band
  stream errors. providers previously had zero async tests; the first one
  caught the regression above.

- **Coverage reporting CI.** A `coverage` job runs `cargo-llvm-cov` to produce
  an lcov report (uploaded as a workflow artifact) plus a textual summary. It
  builds without `-D warnings` and never gates merges.

- **Tag-driven release workflow.** `release.yml` builds release binaries for
  x86_64/aarch64 (linux gnu + musl) and macOS (both arches) on `v*` tags and
  publishes a GitHub release with auto-generated notes.

### Changed

- The `flock`-exclusion test in `neenee-store` is now `#[cfg(unix)]`-gated so
  the suite is Windows-ready, and the `path_scan` cache access in the TUI uses
  `get_or_insert_with` instead of a check-then-`unwrap`.

## [0.6.0] - 2026-06-26

### Added

- **Deterministic read-loop guard + range-aware prune staleness (ADR-0034).**
  A model that re-issues the same `read_file` (one page, or thrashing between
  two pages) without progress no longer spins unchecked. A per-turn guard keeps
  a sliding window of recent read-round signatures and, when one recurs past a
  threshold, injects a hidden anti-anchoring nudge naming the repeated read and
  demanding a different action. Detection is pure signature bookkeeping (no
  inference, no false positives on genuine paging) and the nudge is
  **non-terminating** — `Esc`, `hard_stop_rounds`, and `abort` stay the hard
  backstops. Gated by `[agent] loop_review_enabled` (default on; off for
  sub-agents and `/review`). Accompanying it, prune staleness is now
  range-aware: an earlier read is stale only when a *later* same-file result
  supersedes it — a mutation, or a read that *fully covers* its line range — so
  paging through different regions of one large file no longer self-evicts.

- **Anthropic-compatible `/messages` provider + OpenCode Go relay.** A new
  `Transport::Anthropic` (the `anthropic_compat` provider) speaks the Anthropic
  Messages surface, used by opencode-go's MiniMax/Qwen models and any
  Anthropic-format relay. Per-model `max_tokens` is capped at the model's
  registered output limit (MiniMax M3: 131072) so long agent turns from
  high-output models run untruncated. One `OPENCODE_API_KEY` authenticates a
  single provider hosting many models; `default_model` selects the active model
  id within a multi-model provider.

- **MCP auto-reconnect.** MCP server connections are now wrapped in a
  reconnect-capable `McpServer` handle: a crashed server (stdout closed
  mid-session) is transparently restarted, tool calls retry once on connection
  failure, and a background refresh loop re-discovers tools — no more manual
  restart after a transient MCP server crash.

- **models.dev catalog cache + dynamic model registry.** The model catalog is
  now backed by a cached mirror of models.dev
  (`$XDG_CACHE_HOME/neenee/models-dev.json`), refreshed at startup and every 60
  minutes, with the compiled-in registry as fallback so a missing cache never
  blocks startup.

- **Declarative `[permissions]` rules.** Default "always allow" policies can now
  be pre-declared in `config.toml` (`[[permissions.allow]]` with `tool` +
  `scope`), seeding the allowlist at startup so common tools (e.g. `bash`,
  `read_file`) don't prompt on every fresh install. Runtime "Always" decisions
  still persist to `permissions.json`; config rules are re-applied on every
  start.

- TUI component showcase for rendering/testing individual modals in isolation;
  `question_model` picker; `/mcp-catalog` command.

### Changed

- **`[agent] loop_review_enabled` repurposed.** Previously a deprecated no-op
  (the ADR-0030 semantic review it gated was removed); it now toggles the new
  deterministic read-loop guard's anti-anchoring nudge.

### Removed

- **`LlamaServerProvider`.** The dedicated local provider module is gone:
  `llama-server --jinja` speaks the full OpenAI chat-completions surface
  (native tool calls + streaming tool-call deltas), so local servers are now
  reached through the same `OpenAiCompatProvider` as any cloud endpoint.
  Keyless `Transport::Llama` channels suppress the `Authorization` header
  entirely (an empty bearer token could be rejected by some servers).

## [0.5.0] - 2026-06-25

### Changed

- **Migrated to Rust 2024 edition.** MSRV lowered from 1.88 to 1.85. The 2024
  edition makes `std::env::set_var`/`remove_var` `unsafe`; all test call sites
  are now wrapped in `unsafe` blocks. `resolver = "3"` (MSRV-aware dependency
  resolution) is now implied by the edition.
- **Major dependency upgrades** to the latest ecosystem:
  - `ratatui` 0.26 → **0.30** and `crossterm` 0.27 → **0.29** (API migration:
    `Frame::size()` → `area()`, `set_cursor` → `set_cursor_position`,
    `Buffer::get` → index syntax, `Rect::inner(&Margin)` → `Rect::inner(Margin)`,
    `Backend::Error` is now generic).
  - `reqwest` 0.12 → **0.13** (`query`/`form` are now opt-in features; default
    TLS backend switched to rustls).
  - `rusqlite` 0.32 → **0.40**, `toml` 0.8 → **1**, `pulldown-cmark` 0.10 →
    **0.13** (`Tag::BlockQuote` now carries `Option<BlockQuoteKind>`).
  - `arboard` 3.4 → **3.6**, `dirs`/`directories` 5 → **6**, `insta` → **1.48**.

### Security

- **Replaced the archived `serde_yaml` 0.9 with `yaml_serde` 0.10** (the
  YAML organization's maintained fork), resolving the `RUSTSEC-2024-0320`
  unmaintained-advisory that failed the `security audit` CI job. Applied via
  Cargo package rename so all `use serde_yaml::` imports are unchanged.

### Fixed

- Fixed two CI compile failures under `-D warnings`: an unused `lines` binding
  in `neenee-tools` tests and an un-gated `read_command_output` in
  `neenee-cli` that became dead code on macOS (the function's only callers are
  `#[cfg(target_os = "linux")]`).
- Updated the `create_project` rust scaffold template to emit `edition = "2024"`.

## [0.4.0] - 2026-06-25

### Added

- **`abort` tool + `Tool::affects_control_flow` axis — the model's
  self-initiated emergency escape hatch.** A new `abort` tool lets the model
  stop the program when it detects a stuck state it cannot recover from: a
  tool loop (repeating the same call with identical arguments), a dangerous or
  irreversible operation, or a dead end. Calling it cancels the in-flight turn
  (the same path as `Esc` / `Ctrl+C`) and then triggers a **graceful exit** —
  the session is saved and `SessionEnd` hooks fire before the process and its
  background tasks end. No hard `process::exit`, so nothing is lost.

  This fills the gap left by the removed loop guards (the ADR-0009 equality
  guard and the ADR-0030 loop-review nudge were both deleted), giving the model
  an *active* way out instead of spinning until the user notices. It is gated
  by a new **orthogonal capability axis**, `Tool::affects_control_flow`, not by
  the filesystem-damage ladder (`ToolAccess`): process control is a separate
  concern from filesystem mutation, so the permission broker is bypassed (an
  escape hatch that waits for approval is useless) and **sub-agents are
  excluded from it unconditionally** — a spawned agent must never be able to
  tear down the whole program. `affects_control_flow` joins `requires_user`
  and `spawns_subagent` as the third non-filesystem capability axis; the
  `abort` tool is its first consumer.

- **`read_image` tool + `ToolOutput::Image` — the model can now see images.**
  A new `read_image` tool reads an image file (PNG, JPEG, GIF, WebP), resizes
  it to a sensible resolution, and returns it as a structured
  `ToolOutput::Image`. Because OpenAI Chat Completions tool messages only
  accept string content, the harness peels the image out of the tool result
  and injects it into a follow-up user-role message (the same channel paste-up
  uses) — mirroring how opencode lowers images for OpenAI-Chat providers. This
  works across kimi / GLM / OpenAI / Gemini; the design was cross-checked
  against codex's `view_image` and opencode's `read`. `read_file`'s
  description was also tightened to make its text-only scope unambiguous.

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

### Removed

- **The in-loop loop guards (ADR-0009 equality guard + ADR-0030 loop-review
  nudge) were removed.** Both could reinforce the very read-loops they
  targeted, and the equality guard was trivially bypassed by micro-adjusted
  arguments. This leaves the harness with no automatic intervention against a
  model that repeats identical tool calls — the new `abort` tool (see Added)
  restores an escape hatch, but as a **model-initiated** action rather than a
  harness-enforced hard stop. `Agent::set_loop_review_enabled` is now a no-op
  stub, and `[agent] loop_review_enabled` is accepted but ignored. The opt-in
  `hard_stop_rounds` total-round cap and user `Esc` remain as backstops. (The
  ADR-0030 entries above are retained for history but describe features that no
  longer ship.)

## [0.3.0] - 2026-06-24

> Note: the v0.3.0 tag was cut but its crate-version bump and CHANGELOG entry
> were never landed — crates stayed at `0.2.0` at that tag. This section is
> backfilled at `0.4.0` release time so the history is honest; the crates jump
> straight `0.2.0 → 0.4.0`.

### Added

- **Lifecycle event hooks** — `pre_tool` / `post_tool` / `turn` / `session`
  hooks fire at well-defined points in the agent loop, letting user scripts
  observe or veto behavior. See ADR-0025.
- **SQLite-backed session migrations** — pragmatic, append-only schema
  migrations replace ad-hoc storage evolution. See ADR-0024.
- **Session-tagged turn events (ADR-0017).** Every turn event now flows under
  `AgentResponse::Turn { session_id, event }`, letting a `/btw` side
  conversation stream alongside the primary transcript over one channel.
- **AI session titles (ADR-0022).** A `TITLE` subagent profile generates a
  title on first turn; `/title` regenerates on demand and titles are
  lockable. Empty transcripts fall back to the first message.
- **Relevance-aware, tiered context pruning (ADR-0021 / ADR-0023).** Pre-turn
  pruning is now gated (not every turn), implicit (no `Compacted` notice), and
  selects by relevance (staleness / dedup / forward keep-alive) with tiered
  degradation (truncate → clear) and informative placeholders.
- **Pursuits store, repeat scheduler, `tool_output` and catalog refinements.**

### Changed

- **Agent-design docs restructured:** consolidated subagents documentation, new
  hooks page, context-pruning / context-compaction explanation pages.
- **Model channel abstraction documented (ADR-0002)** and picker recency
  ordering.
- **TUI:** `read_file` offset rendering, snapshot test, theme/layout updates.

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

[Unreleased]: https://github.com/ming2k/neenee/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/ming2k/neenee/releases/tag/v0.6.1
[0.6.0]: https://github.com/ming2k/neenee/releases/tag/v0.6.0
[0.5.0]: https://github.com/ming2k/neenee/releases/tag/v0.5.0
[0.4.0]: https://github.com/ming2k/neenee/releases/tag/v0.4.0
[0.3.0]: https://github.com/ming2k/neenee/releases/tag/v0.3.0
[0.2.0]: https://github.com/ming2k/neenee/releases/tag/v0.2.0
[0.1.0]: https://github.com/ming2k/neenee/releases/tag/v0.1.0
[0.0.1]: https://github.com/ming2k/neenee/releases/tag/v0.0.1
