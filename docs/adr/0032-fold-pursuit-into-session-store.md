# 0032. Fold pursuit persistence into the session store

- **Status:** Accepted
- **Date:** 2026-06-25

## Context

ADR-0031 removed the model-facing pursuit tools. What remains of the pursuit
subsystem is its **persistence layer**: `PursuitStore` (a SQLite table
`thread_pursuits` in `pursuits.db`), `PursuitService` (a facade over it), and a
`PursuitService` field threaded through `Agent`, `TurnContext`,
`InteractiveTurnContext`, `PursuitContext`, and every handler that touches a
pursuit. The persistence stores exactly two useful fields per session —
`objective: String` and `is_complete: bool` — keyed by `thread_id`, which is
itself the session id (`agent.set_thread_id(&session.id())`).

Meanwhile the codebase already has a unified per-session persistence layer:
`SessionStore` (`crates/neenee-store/src/session.rs`). It is event-sourced
(JSONL event log + JSON snapshot), keyed by session id, and it already absorbs
four other per-session state fields with the same shape as pursuit:

| Field on `SessionData` | Event variant | Setter |
|------------------------|---------------|--------|
| `todos: TodoList` | `TodosSet` | `set_todos` |
| `title: Option<String>` | `TitleSet` | `set_title` |
| `loop_checkpoint: Option<PursuitCheckpoint>` | `CheckpointSet` | `set_checkpoint` |
| `last_relief: Option<ContextReliefCheckpoint>` | `ContextReliefCommitted` | `commit_context_relief` |

Pursuit is the odd one out: same key (session id), same shape (a couple of
fields), same lifecycle (read on resume, mutated by slash commands and the
harness), yet it lives in its own SQLite database with its own store, its own
service facade, its own migration system, and its own `PursuitService`
parameter threaded through every turn context. The reason is purely historical:
the `goals.db` SQLite table predates the `SessionData` fields that absorbed
todos/title/checkpoints, and the ADR-0005 crate split moved the store without
folding it into the session layer.

The cost is real:

- `PursuitService` is a parameter on `TurnContext`, `InteractiveTurnContext`,
  `PursuitContext`, `Agent`, the `/pursue` slash handler, the chat handler, the
  side-session builder, and the agent loop. Every turn clones and threads it
  even though the turn already holds an `Arc<SessionStore>` that is keyed by
  the same session id.
- Sub-agents (`subagent_tool`, `session_review`, `session_title`) each open
  their own throwaway in-memory `PursuitStore` just to satisfy `Agent::new`'s
  signature, even though a sub-agent never has a pursuit. This is ceremony for
  a field that is never read.
- Two migration systems (`PRAGMA user_version` for `pursuits.db`, lazy
  `schema_version` for `SessionData`) where one would do.
- Two durable files (`pursuits.db` + `sessions/<id>.jsonl`) for what is one
  session's state.

## Decision

1. **Move the pursuit primitive onto `SessionData`.** Add
   `pursuit: Option<Pursuit>` to `SessionData` (with `#[serde(default)]` so
   legacy snapshots load with `pursuit = None`). Add a `SessionEvent::PursuitSet
   { pursuit: Option<Pursuit> }` variant. Snapshot semantics, mirroring
   `TodosSet` / `TitleSet`: the full `Option<Pursuit>` is stored on every
   change.

2. **Add pursuit methods to `SessionStore`**, mirroring the `set_todos` /
   `set_title` pattern. The methods are:

   - `pursuit(&self) -> Option<Pursuit>` — read the current pursuit.
   - `set_pursuit(&self, pursuit: Option<Pursuit>) -> Result<(), String>` —
     replace the pursuit (or clear it with `None`). Persists snapshot + event.
   - `mark_pursuit_complete(&self) -> Result<Option<Pursuit>, String>` — flips
     `is_complete = true` on the current pursuit, if any. Returns the updated
     pursuit.
   - `update_pursuit_objective(&self, objective: &str) -> Result<Option<Pursuit>, String>` —
     rewrites the objective on the current pursuit, if any.

   `set_pursuit` is the single write path; the other two are conveniences that
   read-modify-write through it. This collapses the six `PursuitService`
   methods (`get_pursuit`, `set_pursuit`, `update_objective`, `clear_pursuit`,
   `mark_complete`, `active_pursuit`) to four `SessionStore` methods, because
   `get_pursuit`→`pursuit`, `clear_pursuit`→`set_pursuit(None)`, and
   `active_pursuit` (which filtered on `!is_complete`) is no longer needed —
   callers that care check `pursuit.is_complete` directly.

3. **Remove `PursuitService`, `PursuitStore`, the `pursuits` module, and
   `pursuits.db`.** Delete `crates/neenee-store/src/pursuits/`. Remove the
   `pursuits_db` path from `paths.rs`. Remove the `PursuitService` /
   `PursuitStore` re-exports from `neenee-store/src/lib.rs`.

4. **Remove the `pursuit_service` field from `Agent`, `TurnContext`,
   `InteractiveTurnContext`, and `PursuitContext`.** These already hold the
   `Arc<SessionStore>` (or, for `Agent`, nothing — the agent does not need a
   session reference for pursuit; the harness reads pursuit from the session at
   turn start and mirrors it into `PursuitState` for the stop-gate). Every
   `pursuit_service` parameter in the call chain (`agent_loop`, `handlers/chat`,
   `handlers/slash`, `side`) is removed; the session is already there.

5. **Remove `Agent::pursuit_service()` accessor.** It is unused in production
   code (verified by grep).

6. **Sub-agents stop constructing a pursuit store.** `Agent::new` drops the
   `pursuit_service` parameter entirely. `subagent_tool`, `session_review`, and
   `session_title` stop opening an in-memory `PursuitStore` and stop passing a
   `PursuitService`. A sub-agent's `PursuitState` is simply empty (no pursuit),
   which is the correct state for a read-only research / review / title agent.

7. **One-time legacy migration.** On startup, after the session store is
   loaded, if `pursuits.db` still exists on disk, read its `thread_pursuits`
   table and fold any `objective` / `is_complete` into the matching
   `SessionData.pursuit` field (keyed by `thread_id` == session id). This is a
   one-shot best-effort migration: the `pursuits.db` file is left in place so a
   downgrade can recover, but it is never read again after the first successful
   migration. The legacy config-key migration (`load_legacy_pursuit_from_config`)
   is preserved — it feeds into `SessionData.pursuit` instead of the store.

8. **The `thread_id` field on `Agent` is no longer needed for pursuit
   lookup.** It remains for other uses (e.g. hook session id) but the pursuit
   read/write path no longer goes through it — the session is the authority.

## Alternatives considered

- **Keep `PursuitService` as a thin wrapper over `SessionStore`.** Rejected.
   That preserves the threading cost (the parameter is still on every context)
   while removing the only thing that justified it (the independent store). A
   wrapper that delegates to `SessionStore` is worse than calling
   `SessionStore` directly.

- **Fold `repeat` into `SessionStore` too.** Rejected. `/repeat` jobs are
   keyed by job UUID, not session id, and a job fires into whichever session is
   active. They are global, not per-session. `SessionStore` is per-session by
   construction. The two are orthogonal and should stay separate.

- **Store pursuit as a special user-role message in the transcript.** Rejected
   for now. It would dissolve the primitive further (no `SessionData` field),
   but it would make `/pursue status` and the resume path grep the transcript
   for a marker, which is fragile and couples the primitive to message
   rendering. A `SessionData` field + event variant is the established pattern
   for per-session state and is the right home.

- **Drop `PursuitCheckpoint` (`loop_checkpoint`) now too.** Tempting (it is
   observability-only per ADR-0015), but it is a separate field with a separate
   lifecycle and out of scope for this ADR. It stays.

## Consequences

Positive:

- One per-session persistence layer. Pursuit, todos, title, checkpoints, and
  relief all live in `SessionData` + `SessionEvent`. No second database, no
  second migration system, no `PursuitService` parameter threading.
- `Agent::new` loses a parameter; sub-agent constructors stop the in-memory
  `PursuitStore` ceremony.
- `TurnContext` / `InteractiveTurnContext` / `PursuitContext` each lose a
  field. The turn already holds the session; pursuit is read from it directly.
- One fewer SQLite file on disk (`pursuits.db` is no longer created for new
  users; the legacy file is migrated and abandoned).
- The pursuit primitive is visibly what it is: two fields on the session, read
  on resume, written by slash commands and the harness. No bespoke store.

Negative:

- Breaking change for anyone reading `pursuits.db` externally. The file is
  migrated and then ignored; external tooling that parsed it must move to the
  session snapshot.
- The one-time migration is best-effort: if `pursuits.db` is corrupt or the
  session snapshot already has a pursuit (e.g. from a prior partial migration),
  the DB row is skipped. A user with an active pursuit across the upgrade will
  see it restored on resume; a user without one sees no change.
- Sub-agents no longer have even a throwaway pursuit store. This is correct
  (they never had a real pursuit) but means any future feature that wanted a
  sub-agent to run a pursuit would need to route through the parent's session.
  That is the right boundary anyway.

Neutral:

- `PursuitState` (the in-memory stop-gate state: `pursuit`, `armed`,
  `iterations`) is unchanged. It is the runtime view; `SessionData.pursuit` is
  the durable view. The harness mirrors the durable view into the runtime view
  at turn start (`refresh_agent_pursuit`) and mirrors the completed runtime
  view back at turn end (`mark_pursuit_complete`). This is the same split as
  `Agent::todos` (runtime) vs `SessionData.todos` (durable).

## References

- [ADR-0010](0010-slim-goal-primitive.md) — slimmed the pursuit primitive to
  `{objective, is_complete}`.
- [ADR-0015](0015-pursue-stop-gate-and-repeat-cron.md) — the stop-gate and
  marker; kept the persistence layer.
- [ADR-0031](0031-pursuit-tools-removed.md) — removed the model-facing pursuit
  tools; this ADR removes the persistence layer that justified them.
- `crates/neenee-store/src/session.rs` — `SessionData`, `SessionEvent`,
  `SessionStore` (the absorbing layer).
- `crates/neenee-store/src/pursuits/` — deleted (`store.rs`, `service.rs`,
  `mod.rs`).
- `crates/neenee-agent/src/orchestration.rs` — turn contexts lose the
  `pursuit_service` field; `execute_turn` reads/writes pursuit through the
  session.
- `crates/neenee-agent/src/agent.rs` — `Agent::new` loses the
  `pursuit_service` parameter.
