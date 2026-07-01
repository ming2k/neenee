# Context pruning

Pruning is the cheapest of the three context-projection layers (pruning →
[compaction](context-compaction.md) → overflow recovery). It clears the
**bodies** of old tool results while leaving the conversation — and the
`tool_call_id` chain — fully intact. Because nothing about the dialogue's shape
changes, pruning is an **implicit** background step: it never surfaces a
transcript notice, only a `debug` trace.

This page is the design reference for pruning. For the summarizing layer that
takes over when pruning is no longer enough, see
[Context compaction](context-compaction.md); for where both sit in the round,
see [Harness architecture](harness.md#context-projection).

## What it relieves

In an agentic loop the bulkiest, most disposable context is **tool output**:
hundreds of lines of `bash` logs, whole files from `read`, search dumps, a
envoy's full transcript. These age badly — an early `grep` result rarely
informs a decision fifty rounds later — yet they keep occupying the window.

Pruning cannot simply delete an old tool message: the OpenAI-style protocol
pairs every `tool_call` with its `tool_result` through a `tool_call_id`, and
dropping one half breaks the chain for providers that validate it. So pruning
**keeps the message and degrades its content in tiers** — it never deletes the
shell. A candidate is either *truncated* (head and tail kept, middle elided) or
*cleared* to an informative placeholder,
`[cleared tool result: read config.rs (42 lines, 1500 chars)]`
(`CLEARED_TOOL_PREFIX`, `neenee-core/src/pressure.rs`), which tells the model
exactly what it lost and lets it decide whether to re-fetch. The legacy
`[Old tool result content cleared]` form (`PRUNED_TOOL_PLACEHOLDER`) is still
recognised on read so older sessions stay idempotent. Either way the
`tool_call_id` chain stays valid; the bytes are reclaimed.

## The core operation

`prune_tool_results` (`neenee-core/src/pressure.rs`) is the pure, side-effect-free
heart of the layer. It plans degradations without mutating, then applies them
atomically — and only when the planned reclaim exceeds `min_reclaim_chars`, so
the durable archive is never churned for a negligible win.

Selection is **not** FIFO-by-age. For each older tool result it decides both
*what* to prune and *how hard*, in five passes:

1. **Recency protection.** Walk newest-first, protecting the most recent
   `protect_recent_chars` of tool output verbatim — that is what is usually
   still relevant.
2. **Staleness / dedup.** Correlate each result back to its originating call via
   `tool_call_id` (name + arguments). An earlier read is stale only when a
   *later* same-file result **supersedes** it: a mutation (`write`/`edit`), or a
   read that **fully covers** its line range (a genuine re-read). Reads of
   *different* pages of one file are complementary, not superseding, so paging
   never self-evicts (ADR-0034). A truly superseded result is cleared outright —
   keeping stale content is worse than clearing it, but evicting a live page just
   makes the model re-read it.
3. **Keep-alive.** A fresh result whose file target is referenced *after* it was
   produced — by later natural language or a later tool call on the same file —
   is left intact, since it is likely still in play. Looking forward from each
   result (not at a global recent window) is what prevents a result's own
   originating call from self-referencing and sparing everything.
4. **Tiered degradation.** A large, fresh candidate is first *truncated* (head +
   tail kept, middle elided) — a gentler tier that preserves the output's shape;
   only on a later, higher-pressure pass is it fully cleared. A small candidate
   (where truncation would not save enough to be worth the lost signal) is
   cleared directly.
5. **Informative clears.** A full clear writes
   `[cleared tool result: <label> (<n> lines, <m> chars)]`, carrying the tool
   name, its salient argument, and the size dropped.

It is **idempotent and convergent**: an already-cleared result is skipped, and a
truncated result escalates to a clear on the next pass, so repeated calls settle.
Nested envoy transcripts descend the same policy: an `envoy` result carries
its child's whole conversation as `children`, which is real weight the parent
pays for, so the same plan recurses into it (bounded by the schema rule that
prevents `envoy` from spawning `envoy`).

## Two entry points, one threshold

Pruning runs at two moments in a round. Both are gated on the **same**
model-relative trigger — `prune_threshold_tokens`, i.e. `prune_utilization`
(0.65) of the active model's context window (see
[compaction](context-compaction.md#thresholds-are-model-relative) for how the
policy resolves). Neither runs unconditionally. Pressure against that threshold
is measured by `effective_pressure_tokens`, which prefers a plausible
provider-reported `prompt_tokens` and otherwise falls back to the conservative
chars/4 estimate — see
[compaction](context-compaction.md#how-pressure-is-measured).

| Entry point | When | Code |
|-------------|------|------|
| **Pre-round** | Once at the start of a round, before the agentic loop begins. Relieves pressure the new user message plus history may already have built up. | `prune_and_commit` in `neenee-agent/src/orchestration.rs` |
| **Mid-round** | Between tool turns *inside* the loop, when a single round fans out across many tool calls and pressure climbs mid-flight. | `MidTurnPruneProjectionGate` (impl of `ContextProjectionGate`), driven by `Agent::project_context_if_needed` |

> **History.** The pre-round entry point originally ran on *every* round with no
> pressure check, so on a 1M-token model it fired at a few percent of the
> window — far below the documented 65%. ADR-0021 gated it to match the
> mid-round gate and the `prune_utilization` design. (The token count that gate
> compares against is measured by the layered policy in
> [ADR-0044](../../adr/0044-layered-token-accounting.md) — see
> [Token accounting](token-accounting.md).)

## Implicit by design

Pruning preserves conversation continuity completely: the round structure and
the `tool_call_id` chain survive, and only stale tool *detail* is lost. It is
therefore **silent** — `prune_and_commit` records a durable checkpoint and a
`tracing::debug!` line, but does **not** emit `AgentResponse::Compacted`. The
mid-round gate was already silent. Only the summarizing
[compaction](context-compaction.md) layer surfaces a transcript notice, so when
a user sees "Compacted …" a real summarization happened, not a prune (ADR-0021).

## Durability

Pruning is lossy in the model-visible window but never in the durable session.
Both entry points commit through `SessionStore::commit_context_projection`,
which:

- extends the session's archived transcript with the pruned originals,
- replaces the model window with the pruned list,
- appends a `SessionEvent::ContextProjectionCommitted` event.

The full pre-prune content is thus recoverable from the durable session even
though the model no longer sees it. This is the shared "archive-and-replace"
mechanism that compaction also uses; it is named `ContextProjection*` because
it records how the durable session is projected into the model window
(ADR-0040).

## Configuration

| Key | Default | Effect |
|-----|---------|--------|
| `compaction_prune` | `true` | Master switch for both entry points. |
| `compaction_prune_protect_tokens` | `6_000` | Recent tool output protected from pruning (converted to chars via `CHARS_PER_TOKEN`). Larger = keep more recent detail, prune less. |
| `[compaction].prune_utilization` | `0.65` | Window fraction at which pruning engages. |

`ContextProjectionSettings::PRUNE_MIN_RECLAIM_CHARS` (8_000) is the fixed
reclaim floor and is not configurable.

## References

- ADR-0009 — uncapped agentic loop; relief as the runaway backstop.
- ADR-0044 — layered token accounting: the provider `usage` path and the
  char-class estimator that measures the tokens these thresholds compare against
  (implements the token-accounting half of the earlier, since-removed layered
  design).
- ADR-0021 — pruning gated at 65% and made implicit.
- ADR-0040 — session state and model-context projection vocabulary.
- ADR-0023 — relevance-aware selection, tiered degradation, informative
  placeholders (the pruning-strategy half; its token-accounting half is now
  [ADR-0044](../../adr/0044-layered-token-accounting.md)).
- [Token accounting](token-accounting.md) — how the token count is measured.
- [Context compaction](context-compaction.md) — the next, heavier relief layer.
- `neenee-core/src/pressure.rs` — `prune_tool_results`, `CompactionPolicy`,
  `ContextBudget`.
