# 0021. Tool-result pruning is implicit and distinct from compaction

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

ADR-0019 made context-relief thresholds model-relative and described a
three-layer escalation ladder: cheap **tool-result pruning** at
`prune_utilization` (0.65), then **summarizing compaction** at `utilization`
(0.85), then reactive overflow recovery. Pruning clears the *bodies* of old
`Tool`-role results to a placeholder while keeping the message and its
`tool_call_id` intact; compaction summarizes whole older turns into a checkpoint
and archives the originals.

Three problems remained in the implementation:

1. **Pruning was not actually gated.** The mid-turn relief gate
   (`Agent::relieve_pressure_if_needed`, `agent.rs`) honored
   `prune_threshold_tokens`, but the **pre-turn** prune
   (`orchestration.rs`) ran on every turn — `if compaction.prune` with no
   pressure check. On a 1M-token model it fired at a few percent of the window,
   not the documented 65%, contradicting ADR-0019's `prune_utilization`.

2. **Pruning wore compaction's face in the UI.** `prune_and_commit` emitted the
   same `AgentResponse::Compacted` event as a real summarizing compaction, and
   the TUI rendered both with one fixed string, `"Compacted N messages: X -> Y
   chars."`. A user could not tell a cheap, continuity-preserving prune from an
   expensive, lossy summarization. Pruning does not break conversation
   continuity (the `tool_call_id` chain and turn structure survive), so making
   it shout in the transcript was misleading.

3. **Pruning wore compaction's name in the code.** The shared persistence
   mechanism — `CompactionCheckpoint`, `CompactionResult`, `commit_compaction`,
   `SessionEvent::CompactionCommitted`, the `CompactionGate` trait, and
   `MidTurnCompactionGate` — was named "compaction" even though pruning is its
   most frequent caller. The vocabulary conflated two different operations.

## Decision

1. **Gate pre-turn pruning** on `prune_threshold_tokens`, mirroring the mid-turn
   gate, so both prune entry points engage only above ~65% of the window.

2. **Make pruning implicit.** `prune_and_commit` no longer sends
   `AgentResponse::Compacted`; it records a durable checkpoint and a
   `tracing::debug!` line only. `Compacted` is now emitted **exclusively** by
   the summarizing-compaction paths, so the transcript notice and the
   `Compaction`/`Compacted` names are accurate.

3. **Rename the shared mechanism to neutral `ContextRelief*` vocabulary**, since
   it serves both prune and compact:
   - `CompactionCheckpoint` → `ContextReliefCheckpoint`
   - `CompactionResult` → `ContextReliefResult`
   - `commit_compaction` → `commit_context_relief`
   - `SessionEvent::CompactionCommitted` → `ContextReliefCommitted`
   - `CompactionGate` → `ContextReliefGate`; `MidTurnCompactionGate` →
     `MidTurnPruneGate` (it is specifically the prune gate)
   - `SessionData.compaction` field / `SessionStore::compaction()` accessor →
     `last_relief`
   Genuinely compaction-specific symbols keep the name: `compact_turn_history`,
   `run_compaction`, `CompactionSelection`, `CompactionPolicy`,
   `CompactionSettings`, `CompactionDecision`, `CompactionHooks`,
   `AgentResponse::Compacted`, `send_compaction`.

## Alternatives considered

- **Leave pruning ungated as a cheap every-turn pass.** Rejected: it diverges
  from ADR-0019's documented `prune_utilization`, churns the durable archive on
  turns with no real pressure, and surprised the user who reasonably expected
  relief near the configured threshold, not at a few percent.

- **Give pruning its own `Pruned` event and a distinct transcript notice**
  instead of going silent. Rejected for the default path: pruning does not
  affect conversation continuity, so a notice is noise. The `debug` trace keeps
  it observable when explicitly sought.

- **Duplicate the persistence layer into `Prune*` and `Compaction*` types.**
  Rejected: the archive-and-replace operation is genuinely identical for both;
  duplicating it would add redundancy for no gain. A single neutral
  `ContextRelief*` mechanism with the caller deciding visibility is the honest
  factoring.

## Consequences

- **Positive.** Pruning now fires at the documented ~65% on every model;
  pruning and compaction are unambiguous in both code and UI; the `Compacted`
  notice means a real summarization happened.

- **Backward compatibility.** The renamed on-disk forms keep serde aliases:
  `ContextReliefCommitted` aliases the old `compaction_committed` event tag, and
  the `last_relief` field aliases the old `compaction` key, so session
  snapshots and event logs written before the rename still load. A regression
  test (`events::tests::legacy_compaction_committed_tag_still_replays`) guards
  the event path.

- **Neutral.** `ContextReliefCheckpoint` still reports `before_chars` /
  `after_chars`; the `/session` info line now reads "Last context relief"
  instead of "Last compaction" since the stored checkpoint may be from either
  layer.

## References

- ADR-0009 — uncapped agentic loop; relief as the runaway backstop.
- ADR-0019 — model-relative thresholds; the `prune_utilization` design pruning
  now actually honors.
- `neenee-agent/src/orchestration.rs` — `prune_and_commit`, `MidTurnPruneGate`,
  pre-turn gate.
- `neenee-store/src/session.rs`, `neenee-store/src/events.rs` — the renamed
  relief persistence mechanism and serde aliases.
- `docs/explanation/agent-design/harness.md` — three-layer relief description.
