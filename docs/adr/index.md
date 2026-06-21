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
| [0002](0002-model-channel-abstraction.md) | Model/channel abstraction and picker redesign | Proposed |
| [0003](0003-extract-neenee-app-crate.md) | Extract `neenee-app` from the binary crate | Superseded by ADR-0004 |
| [0004](0004-six-crate-topology.md) | Six-crate topology: core / app / providers / tools / harness / cli | Superseded by ADR-0005 |
| [0005](0005-strict-layering-and-renames.md) | Strictly-layered topology + scenario-bound store | Accepted |
| [0006](0006-plan-mode-v2.md) | Plan mode v2: approval gate, active plan path, proposed-plan rendering | Accepted |
| [0007](0007-plan-progress-panel.md) | Plan progress sticky panel above input box | Accepted |
| [0008](0008-single-breathing-anchor.md) | Single breathing anchor for TUI liveness | Accepted |
| [0009](0009-uncapped-agentic-loop.md) | Uncapped agentic loop (remove per-turn round cap and `/loop` iteration cap) | Accepted |
| [0010](0010-slim-goal-primitive.md) | Slim the goal primitive (drop status machine, token budget, time accounting) | Accepted |
| [0011](0011-subagent-profiles.md) | Sub-agent profiles: capability-axis tool admission (`requires_user` / `spawns_subagent` + `EXPLORE` profile) | Accepted |
| [0012](0012-toolaccess-tier-split.md) | `ToolAccess` tier split (`Read < Execute < Write`) and the `VERIFY` profile | Accepted |
