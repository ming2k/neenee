# Architecture Decision Records

Durable records of significant technical decisions and their context. Each ADR
is a short Markdown file numbered `NNNN-<slug>.md`. Once a decision is final its
status is `Accepted`; a later ADR supersedes an earlier one rather than editing
it in place.

See [ADR Workflow](../dev/documentation/adr-workflow.md) for the process.

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-tool-rendering-redesign.md) | Tool-step rendering redesign: log entries over expandable cards | Accepted |
| [0002](0002-model-channel-abstraction.md) | Model/channel abstraction and picker redesign | Accepted |
| [0003](0003-extract-neenee-app-crate.md) | Extract `neenee-app` from the binary crate | Superseded by ADR-0004 |
| [0004](0004-six-crate-topology.md) | Six-crate topology: core / app / providers / tools / harness / cli | Superseded by ADR-0005 |
| [0005](0005-strict-layering-and-renames.md) | Strictly-layered topology + scenario-bound store | Accepted |
| [0006](0006-plan-mode-v2.md) | Plan mode v2: approval gate, active plan path, proposed-plan rendering | Superseded by ADR-0027 |
| [0007](0007-plan-progress-panel.md) | Plan progress sticky panel above input box | Superseded by ADR-0020 |
| [0008](0008-single-breathing-anchor.md) | Single breathing anchor for TUI liveness | Accepted |
| [0009](0009-uncapped-agentic-loop.md) | Uncapped agentic loop (remove per-turn round cap and `/loop` iteration cap) | Accepted |
| [0010](0010-slim-goal-primitive.md) | Slim the goal primitive (drop status machine, token budget, time accounting) | Accepted |
| [0011](0011-subagent-profiles.md) | Sub-agent profiles: capability-axis tool admission (`requires_user` / `spawns_subagent` + `EXPLORE` profile) | Accepted |
| [0012](0012-toolaccess-tier-split.md) | `ToolAccess` tier split (`Read < Execute < Write`) and the `VERIFY` profile | Accepted |
| [0013](0013-skills-xdg-paths-and-bundled-embed.md) | Skills & commands: XDG paths + compile-time-embedded bundled skills | Accepted |
| [0014](0014-xdg-persistence-architecture.md) | Unified XDG persistence architecture (single `Dirs` policy, four-category model, fixed override precedence) | Accepted |
| [0015](0015-pursue-stop-gate-and-repeat-cron.md) | Pursue stop-gate + repeat cron scheduler (replace `/goal` + `/loop`) | Accepted |
| [0016](0016-session-review-over-round-counting.md) | Session review over round-counting stall detection (diagnostic sub-agent + opt-in hard stop) | Accepted |
| [0017](0017-side-conversations.md) | Side conversations: session-native `/btw` (concurrent parent + side, event envelope, `fork_to_side`) | Accepted |
| [0018](0018-per-project-multi-instance-concurrency.md) | Per-project multi-instance concurrency (drop the per-project flock; one session file per live instance; file-scoped locks on shared global state) | Accepted |
| [0019](0019-model-relative-context-compaction.md) | Model-relative context compaction (token thresholds derived from the active model's context window) | Accepted |
| [0020](0020-unified-task-list.md) | Unified task list (supersede the per-plan progress panel with one shared `TodoList`) | Accepted |
| [0021](0021-pruning-is-implicit-and-distinct-from-compaction.md) | Tool-result pruning is implicit (silent, gated at ~65%) and renamed distinct from compaction (`ContextRelief*`) | Accepted |
| [0022](0022-session-level-ai-title.md) | Session-level AI title (first-turn auto + on-demand `/title` + manual lock; `TITLE` sub-agent profile) | Accepted |
| [0023](0023-relevance-aware-tiered-pruning-and-layered-token-accounting.md) | Relevance-aware, tiered pruning (staleness/keep-alive, truncate→clear, informative placeholders) and layered token accounting (`effective_pressure_tokens`) | Accepted |
| [0024](0024-pragmatic-sqlite-migrations.md) | Pragmatic SQLite migrations via `PRAGMA user_version` | Accepted |
| [0025](0025-lifecycle-event-hooks.md) | Lifecycle event hooks (single event axis: SessionStart/End, UserPromptSubmit, PreToolUse/PostToolUse/Stop/PreCompact; command-handler config; delete one-shot `CompactionHooks`) | Accepted |
| [0026](0026-plan-progression-forcing-functions.md) | Plan progression forcing functions (plan-exit nudge, todo-continuation nudge, approval-handoff instruction; re-order verify-nudge after the todo list drains) | Superseded by ADR-0033 |
| [0027](0027-plan-as-subagent.md) | Plan as a subagent (replace Plan mode with a `PLAN` profile + a `plan` tool; supersedes ADR-0006, revises ADR-0026; depends on ADR-0028/0029) | Superseded by ADR-0033 |
| [0028](0028-capability-allocation-scoped-writes.md) | Capability allocation: scoped filesystem writes (`WriteScope` per agent + `write_paths` grant on `ToolPolicy`; decouples write admission from the `ToolAccess` ceiling) | Accepted |
| [0029](0029-full-duplex-subagent-communication.md) | Full-duplex subagent communication (steering `AgentOp` inbox + `SubagentHandle` request/reply + `SubagentRegistry` keyed by parent tool-call id) | Accepted |
| [0030](0030-early-loop-intervention-and-round-hook.md) | Early loop intervention (in-loop semantic review + anti-anchoring nudge via a `steering` module) and a constrained `Deny`-forbidden round-count hook (partially supersedes ADR-0025) | Accepted |
| [0031](0031-pursuit-tools-removed.md) | Remove the pursuit tools (`get_pursuit` / `start_pursuit` / `complete_pursuit`); the `/pursue` slash command, stop-gate, and `[NEENEE_PURSUIT_COMPLETE]` marker own the lifecycle (reverses the tool-keeping sub-decisions of ADR-0010/0015) | Accepted |
| [0032](0032-fold-pursuit-into-session-store.md) | Fold pursuit persistence into `SessionStore` (delete `PursuitStore` / `PursuitService` / `pursuits.db`; move `pursuit` onto `SessionData` + `SessionEvent::PursuitSet`; drop the `pursuit_service` field from `Agent` and every turn context) | Accepted |
| [0033](0033-remove-plan-and-verify-workflow.md) | Remove the plan-as-subagent and verify workflow (`plan` / `verify_plan_execution` tools, `PLAN` / `VERIFY` profiles, plan-exit / todo-continuation / verify nudges, `MAX_REPEATED_TOOL_CALLS`; supersedes ADR-0026 / ADR-0027, narrows ADR-0012) | Accepted |
| [0034](0034-range-aware-pruning-and-deterministic-read-loop-guard.md) | Range-aware prune staleness (a read is stale only when a later read covers its line range, or the file is mutated — paging no longer self-evicts; refines ADR-0023) and a deterministic, non-terminating read-loop guard (frequency-window detection + anti-anchoring nudge; revives ADR-0030's intervention without a model call) | Accepted |
| [0035](0035-application-layer-split.md) | Application-layer split: `neenee-code` + `neenee-quant` (rename cli→code; add the quant application crate + `QUANT` profile) | Accepted |
| [0036](0036-cjk-wide-char-ghost-cells.md) | Heal CJK wide-character "ghost" cells with a whole-row re-emitting backend wrapper (`WideHealBackend`) so wide-glyph trailing columns stay fresh through tmux | Superseded by ADR-0038 |
| [0037](0037-server-layer.md) | Session/server layer (`neenee-server`): multi-session daemon + multi-frontend (`SharedState` / `SessionRegistry` / `SessionHandle`; `mpsc`→`broadcast`) | Accepted |
| [0038](0038-in-house-grid-diff-rendering-engine.md) | Replace ratatui with an in-house grid + diff rendering engine (`neenee-tui`: vim-style retained grid, write-marks-dirty diff, crossterm backend, `bce` awareness); supersedes ADR-0036 | Accepted |
| [0039](0039-unified-prompt-registry.md) | Unified prompt registry: declarative system-channel composition via `PromptSection` (one trait + one registry keyed by `InjectionKind` replace the ad-hoc `format!`/`push_str` system-prompt assembly; the two duplicated turn-loop prep funnels collapse to one; two latent sub-agent system-message clobber defects fixed). User channel and store prompts investigated and deliberately not migrated | Accepted |
| [0040](0040-session-state-and-context-projection.md) | Session state and model-context projection vocabulary (`SessionData.model_window`, `archived_transcript`, `ContextProjection*`; legacy `ContextRelief*` aliases retained on disk) | Accepted |
| [0041](0041-tool-capabilities-scope-and-override.md) | Tool capabilities and variants: two orthogonal axes — scope (agent/profile: which capabilities) vs override (model: which variant via `[tool_variants]`); `ToolSet`/`Capability` registry; subagents inherit the parent model's variant selection; replaces the `[tool_overrides]` string-patch layer | Accepted |
| [0042](0042-principal-envoy-role-vocabulary.md) | Principal / Envoy role vocabulary: keep `agent` as the umbrella engine term; name the top-level role `Principal` (`[principal]` config) and the spawned child role `Envoy` (renames the `subagent` tool/types/files); hard rename, no config alias | Accepted |
| [0043](0043-bash-stdin-execution-contract.md) | Bash stdin execution contract: non-interactive by construction (`StdinPolicy` hard floor + idle watchdog + interactive classifier + `\r`-aware capture + themed termination footer + human/model input injection) | Accepted |
| [0044](0044-layered-token-accounting.md) | Layered token accounting: provider `usage` is authoritative when present (`usage_supported`/`take_last_usage` + `ProviderStreamEvent::Usage`), falling back to a char-class estimator that is accurate for CJK + code (replaces `bytes/4`); a `TokenSourceLedger` attributes every token as reported vs estimated and a report modal surfaces the accuracy | Accepted |
| [0045](0045-extract-neenee-tui-view.md) | Extract `neenee-tui-view` (widgets + semantic document model) from the `neenee-code` app shell; three-layer engine/view/shell topology with a one-way `TranscriptView<'a>` seam enforced by the compiler | Accepted |
