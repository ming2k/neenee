# 0020. Unified task list (supersedes ADR-0007)

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

ADR-0007 introduced a **per-plan progress panel**: when `plan_exit` was
approved, the plan markdown was parsed into `##` sections and tracked through a
dedicated `PlanProgress` type + `update_plan_progress` tool, persisted in its
own session field, and rendered in its own sticky TUI panel.

Separately, a scratchpad `todo` tool existed for ad-hoc task tracking. Over
time these became **two parallel implementations of the same feature** — "an
ordered list of named items each with a status" — with duplicated parsing
(`parse_plan_headings`), duplicated status enums (`PlanSectionStatus` vs
`TodoStatus`), duplicated persistence (`PlanProgressSet` vs `TodosSet` events,
`plan_progress` vs `todos` session fields), duplicated events
(`PlanProgressUpdated` vs `TodosUpdated`), and duplicated rendering paths.

The `todos.rs` module doc already declared the intent to supersede
`PlanProgress`, but the migration was never completed: `plan.rs` still owned
the full progress machinery, `plan_exit` still seeded a `PlanProgress`, and the
`update_plan_progress` tool was still registered alongside `todo` /
`todo_update`. The TUI rendered `plan_progress` and never rendered `todos` at
all. The overlap was a recurring source of confusion about which surface was
authoritative.

## Decision

Collapse the two into **one** unified task list. The `todos` module is the
single source of truth; the `plan` module keeps only the Plan *Mode* workflow
(`plan_enter` / `plan_exit` / plan-path resolution).

Concretely:

- **Remove** `PlanProgress`, `PlanSection`, `PlanSectionStatus`,
  `UpdatePlanProgressTool`, `PLAN_STALE_TURN_THRESHOLD`, and the plan-local
  `parse_plan_headings` from `crates/neenee-core/src/plan.rs`.
- **`PlanToolContext` embeds a `TodoToolContext`** (the same shared
  `Arc<Mutex<TodoList>>` + turn counter the `todo` / `todo_update` tools and
  the `Agent` own), so `plan_exit` seeds the list and `plan_enter` clears it
  through the one cell everyone reads.
- **`plan_exit` seeds a `TodoList`** via `TodoList::from_plan_markdown` (one
  `pending` item per `##` heading); **`plan_enter` clears it**.
- **Drop** the `PlanProgressUpdated` agent/UI events and the `PlanProgressSet`
  session event + `plan_progress` session field. Only `TodosUpdated` /
  `TodosSet` remain.
- The TUI renders the list (in the Activity modal) and emits transcript
  notices from `TodosUpdated`, exactly as it did from `PlanProgressUpdated`.
- The Build-mode prompt tells the model to track steps with `todo` /
  `todo_update` instead of `update_plan_progress`.

## Alternatives considered

- **Keep both, document the boundary.** Rejected: the boundary is artificial —
  both track "ordered named items with status," and maintaining two surfaces
  forever guarantees drift (the TUI never even rendered `todos`).
- **Have `plan_exit` seed `PlanProgress` and bridge it into `TodoList`.**
  Rejected: bridging keeps the duplicate type alive and forces a lossy
  translation layer between two near-identical models.

## Consequences

- **Positive:** one list, one panel, one persisted field, one event, one set of
  status semantics. A plan and ad-hoc task tracking are now genuinely the same
  feature. The `plan` module shrinks to its actual responsibility (mode +
  plan-path workflow).
- **Positive:** `todo_update` (mark by position or content substring) subsumes
  `update_plan_progress`'s substring-matching with a richer, identity-stable
  reconcile model.
- **Negative / migration:** the `update_plan_progress` tool and the
  `plan_progress` session field / `PlanProgressSet` event are removed. Old
  sessions load with graceful degradation by design: unknown session fields are
  ignored by serde, a dropped field triggers at most a checksum *warning* (not
  a rejection), and unrecognized `plan_progress_set` event-log lines are
  skipped. The previously persisted progress is simply not restored.
- **Negative:** ADR-0007's per-plan panel design is no longer in effect; this
  ADR supersedes it. The old ADR text is left intact as a historical record.

## References

- Supersedes [ADR-0007](0007-plan-progress-panel.md).
- [ADR-0006](0006-plan-mode-v2.md) — Plan-mode workflow this plugs into.
- `crates/neenee-core/src/todos.rs`, `crates/neenee-core/src/plan.rs`.
