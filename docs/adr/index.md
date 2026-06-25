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
| [0026](0026-plan-progression-forcing-functions.md) | Plan progression forcing functions (plan-exit nudge, todo-continuation nudge, approval-handoff instruction; re-order verify-nudge after the todo list drains) | Accepted |
| [0027](0027-plan-as-subagent.md) | Plan as a subagent (replace Plan mode with a `PLAN` profile + a `plan` tool; supersedes ADR-0006, revises ADR-0026; depends on ADR-0028/0029) | Accepted |
| [0028](0028-capability-allocation-scoped-writes.md) | Capability allocation: scoped filesystem writes (`WriteScope` per agent + `write_paths` grant on `ToolPolicy`; decouples write admission from the `ToolAccess` ceiling) | Accepted |
| [0029](0029-full-duplex-subagent-communication.md) | Full-duplex subagent communication (steering `AgentOp` inbox + `SubagentHandle` request/reply + `SubagentRegistry` keyed by parent tool-call id) | Accepted |
| [0030](0030-early-loop-intervention-and-round-hook.md) | Early loop intervention (in-loop semantic review + anti-anchoring nudge via a `steering` module) and a constrained `Deny`-forbidden round-count hook (partially supersedes ADR-0025) | Accepted |
| [0031](0031-pursuit-tools-removed.md) | Remove the pursuit tools (`get_pursuit` / `start_pursuit` / `complete_pursuit`); the `/pursue` slash command, stop-gate, and `[NEENEE_PURSUIT_COMPLETE]` marker own the lifecycle (reverses the tool-keeping sub-decisions of ADR-0010/0015) | Accepted |
| [0032](0032-fold-pursuit-into-session-store.md) | Fold pursuit persistence into `SessionStore` (delete `PursuitStore` / `PursuitService` / `pursuits.db`; move `pursuit` onto `SessionData` + `SessionEvent::PursuitSet`; drop the `pursuit_service` field from `Agent` and every turn context) | Accepted |
