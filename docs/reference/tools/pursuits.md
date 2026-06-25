# Pursuit interface

A pursuit is a durable, per-session objective: `{ objective, is_complete }`
persisted as a field on `SessionData` via `SessionStore` (ADR-0032) — not a
separate database. There is no status machine, no token/time budget, and no
checklist (ADR-0010). The lifecycle is driven by three mechanisms owned by
three distinct roles — there are no model-facing pursuit tools (ADR-0031):

| Role | Responsibility | Mechanism |
|------|----------------|-----------|
| **User** | Set the condition (entry) | `/pursue <condition>` slash command |
| **Harness** | Drive + gate (continuation) | stop-gate re-injects the condition each round |
| **Model** | Signal completion (exit) | `[NEENEE_PURSUIT_COMPLETE]` marker |

## Entry: `/pursue <condition>`

A slash command (`crates/neenee-code/src/handlers/slash.rs`), not a tool. It
persists the condition via `SessionStore::set_pursuit`, arms the stop-gate on
the agent, and drives one turn via `orchestration::start_pursuit`. The model
cannot start a pursuit on its own — by architecture, not by prompt constraint.

## Continuation: the stop-gate

`/pursue` arms a stop-gate on the `Agent`. At each turn-loop exit, if a pursuit
is armed, an active (incomplete) pursuit exists, the latest response did not
signal completion, and the 50-round safety cap (`MAX_PURSUIT_ITERATIONS`) is not
exhausted, the gate re-injects the condition as a hidden user message and forces
another round instead of returning. See
[Pursuits and the pursue stop-gate](../../explanation/agent-design/pursuits.md).

## Exit: `[NEENEE_PURSUIT_COMPLETE]`

The sole completion signal. The working model emits the
`[NEENEE_PURSUIT_COMPLETE]` marker in an assistant message; the gate sees it and
lets the turn end; orchestration finalizes by calling
`session.mark_pursuit_complete()`. The marker is always stripped from visible
output — it is a control signal, not prose. There is no `complete_pursuit` tool;
the marker is the single path. (`/pursue done` remains the user-driven
completion slash command.)

## See also

- [Pursuits and the pursue stop-gate](../../explanation/agent-design/pursuits.md)
  — the primitive, the stop-gate mechanism, and the `/repeat` comparison
- [ADR-0010](../../adr/0010-slim-goal-primitive.md) — slimmed the pursuit
  primitive
- [ADR-0015](../../adr/0015-pursue-stop-gate-and-repeat-cron.md) — introduced
  the stop-gate and the marker
- [ADR-0031](../../adr/0031-pursuit-tools-removed.md) — removed the
  model-facing pursuit tools
- [ADR-0032](../../adr/0032-fold-pursuit-into-session-store.md) — folded
  pursuit persistence into `SessionStore`
