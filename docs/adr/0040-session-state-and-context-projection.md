# 0040. Session state and model-context projection vocabulary

- **Status:** Accepted
- **Date:** 2026-06-28

## Context

ADR-0021 renamed the shared prune/compact persistence mechanism to
`ContextRelief*`. That name removed the false impression that every archive
and replace was a summarizing compaction, but it still described the runtime
benefit rather than the data boundary.

The persistence boundary needs a stricter term. The session store is
responsible for the complete recoverable scene: the durable transcript, archived
originals, model-visible window, pursuit state, title, task list, and the
metadata describing any prune or compact operation already applied. The provider
request is only a projection of that scene. On resume, the store must restore
the previously committed projection; it must not rediscover, rerun, or guess a
prior prune or compact.

## Decision

Use **model-context projection** as the canonical term for the archive-and-
replace operation that produces the next model-visible window.

- Rename `ContextReliefCheckpoint` to `ContextProjectionCheckpoint`.
- Rename `ContextReliefResult` to `ContextProjectionResult`.
- Rename `SessionEvent::ContextReliefCommitted` to
  `SessionEvent::ContextProjectionCommitted`.
- Rename `SessionStore::commit_context_relief` to
  `SessionStore::commit_context_projection`.
- Rename the mid-turn gate from `ContextReliefGate` to
  `ContextProjectionGate`.
- Rename the current durable window from `SessionData.messages` to
  `SessionData.model_window`.
- Rename the durable archived originals from `SessionData.archived_messages` to
  `SessionData.archived_transcript`.
- Rename `last_relief` to `last_projection`.

Each `ContextProjectionCheckpoint` records an `operation`:

- `prune` for tool-result pruning.
- `compact` for summarizing compaction.
- `unknown` for legacy events and snapshots written before this ADR.

The old on-disk names remain accepted aliases:

- `messages` loads as `model_window`.
- `archived_messages` loads as `archived_transcript`.
- `last_relief` and `compaction` load as `last_projection`.
- `context_relief_committed` and `compaction_committed` load as
  `context_projection_committed`.
- Legacy event fields `archived` and `active` load as `archived_originals` and
  `model_window`.

New writes use the new vocabulary.

## Alternatives considered

- **Keep `ContextRelief*` and only add `operation`.** Rejected. The missing
  operation was a real schema gap, but the term still made the session store
  sound like a runtime pressure valve instead of the durable authority for the
  model-visible projection.
- **Call the durable field `active_messages`.** Rejected. "Active" is vague:
  active for the UI, active for resume, and active for the provider can diverge.
  `model_window` states which consumer the projection serves.
- **Drop legacy aliases.** Rejected. The rename must be a schema migration, not
  a session-breaking reset.

## Consequences

- Resume restores both the full recoverable scene and the already committed
  model-visible window.
- `/session status` reports "Last context projection" and includes whether the
  projection was a prune, compact, or legacy unknown operation.
- New snapshots and event-log lines use the projection vocabulary while older
  session files continue to load.
- ADR-0021 remains the history of why pruning is silent and distinct from
  compaction; this ADR supersedes only its `ContextRelief*` vocabulary.

## References

- ADR-0019 — model-relative context thresholds.
- ADR-0021 — pruning is implicit and distinct from compaction.
- ADR-0023 — relevance-aware tiered pruning.
- Context pruning explanation.
- Context compaction explanation.
