# 0023. Relevance-aware, tiered pruning and layered token accounting

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

ADR-0021 made tool-result pruning implicit (silent, gated at ~65%) and renamed
the shared archive-and-replace mechanism `ContextRelief*` so pruning and
summarizing compaction are distinct. But the pruning *policy itself* was still
the original one inherited from before ADR-0019: **pure FIFO-by-age** — walk the
tool results newest-first, protect a recent char budget, and replace every older
result's content with one fixed placeholder
(`"[Old tool result content cleared]"`).

That policy had four gaps relative to what mature agentic harnesses do, each
independent of the gating fix in ADR-0021:

1. **Token accounting trusted a chars/4 proxy with no provider feedback.**
   `estimate_tokens` divides total chars by 4. For code, JSON, and CJK text the
   real ratio varies by 2–3×, so the 65% / 85% thresholds were an "estimate of an
   estimate": pressure could be 90% real before the gate tripped, or trip early
   and waste a prune. ADR-0019 acknowledged this as a deferred epic.

2. **Selection was FIFO, ignoring relevance.** A `read` of a file being actively
   edited and a one-shot `ls` were pruned in the same order they arrived. An
   earlier `read` superseded by a later `edit` of the same file was kept
   verbatim — stale content, which is worse than no content once a newer state
   exists.

3. **Degradation was a cliff.** A 2000-line file read went from full content to a
   one-line placeholder in a single step, shedding all signal at once.

4. **The placeholder carried no clue about what was lost.** The model saw
   `"[Old tool result content cleared]"` and could not tell whether re-fetching
   was worth it, so it either guessed or blindly re-ran the tool.

## Decision

Pruning stays the cheap, implicit, ~65%-gated first layer from ADR-0021. What
changes is *what* it chooses to prune, *how hard*, and *how pressure is counted`.

### 1. Layered token accounting (`effective_pressure_tokens`)

> **Update (2026-06-24): reverted as dead code.** `effective_pressure_tokens`
> and `USAGE_TRUST_FLOOR` were removed. The `Provider` trait still does not
> surface usage, so the function never had a production caller — `reported` was
> hard-`None` at every site — and an exported, tested function that only
> *advertised* usage-aware accounting was a net liability (it implied a
> capability that does not exist). Pressure is measured purely by
> `estimate_tokens`. The policy below is retained as the design of record:
> when a provider actually reports `prompt_tokens`, reintroduce this prefer-when-
> plausible function at the (then-real) call sites. Until then there is nothing
> to centralize.

`estimate_tokens` (chars/4) is no longer the sole pressure signal. A new
`effective_pressure_tokens(estimate, reported_prompt_tokens)`
(`neenee-core/src/pressure.rs`) prefers a provider-reported `prompt_tokens` when
one is present **and plausible** — at least half the independent estimate — and
otherwise falls back to the chars/4 proxy. The bias is deliberately
conservative: under-counting risks overflow (the expensive failure), while
over-counting only prunes slightly early.

"Plausible" exists because relays and local servers sometimes report `0` or
absurdly small usage; trusting that would under-count and risk overflow. Today
every call site passes `reported = None` — the `Provider` trait does not yet
surface usage, and threading it through the streaming adapters is a separate
epic — but the policy is centralized so wiring real usage later is a one-line
change per call site, not a logic rewrite. Provider-reported usage is a
**correction signal**, never sole ground truth: the independent estimate never
goes away.

### 2. Relevance-aware selection, not FIFO

`prune_tool_results` now plans degradations (without mutating), choosing per
candidate:

- **Recency protection** (unchanged): the most recent `protect_recent_chars` of
  tool output stays verbatim.
- **Staleness / dedup**: each result is correlated to its originating call via
  `tool_call_id`. If a *later* tool touched the same file, an earlier result for
  that file is stale and is cleared outright.
- **Keep-alive**: a fresh result whose file target is referenced *after* it was
  produced — by later natural language or a later tool call on the same file — is
  left intact. Looking forward from each result (not at a global recent window)
  is the key detail: it prevents a result's own originating call from
  self-referencing and sparing everything.

File correlation is heuristic — `tool_call_id` → the assistant call's
`arguments`, matched on the usual path keys (`path`/`file_path`/`file`/
`filename`). Tools that are not file-addressed (`bash`, `grep` without a file)
get no `file_key`, so staleness and keep-alive do not apply and they fall back to
recency-only — the safe default when relevance cannot be inferred.

### 3. Tiered degradation

A candidate is no longer cleared in one step. Large, fresh results are first
**truncated** (head + tail kept, middle elided with a recognizable marker),
preserving the output's shape; only on a later, higher-pressure pass is a
truncated result fully cleared. Small results (where truncation would not save
enough to be worth the lost signal) clear directly. A result shorter than the
placeholder that would replace it is left untouched — clearing it would *grow*
the window.

### 4. Informative placeholders

A full clear writes
`[cleared tool result: <label> (<n> lines, <m> chars)]`, where `<label>` is the
tool name plus its salient argument (`read src/config.rs`, `grep "TODO"`). The
model can now judge whether to re-fetch instead of guessing. The legacy
`[Old tool result content cleared]` form is still recognized on read so sessions
written before this ADR stay idempotent.

The whole operation remains **idempotent and convergent**: already-cleared
results are skipped; truncated results escalate to clears; repeated passes
settle. Nested sub-agent transcripts descend the same policy.

## Alternatives considered

- **Trust provider usage unconditionally.** Rejected. Relays/local servers
  report `0` or nonsense often enough that "usage is ground truth" would
  under-count and overflow. The conservative floor keeps the estimate as a
  backstop. (See ADR-0019's note that real-usage feedback was deferred partly
  because provider reliability is uneven.)

- **Replace the estimate with a local tokenizer.** Deferred, not rejected. A
  local tokenizer is deterministic and provider-independent, but per-model-family
  tokenizers (o200k/cl100k, GLM/Kimi SentencePiece, …) are heavy to maintain and
  unknown/local models still have none — so the estimate backstop stays
  regardless. Layered accounting is the cheaper, model-agnostic step that can be
  taken now; a local tokenizer can refine the estimate later without changing
  this design.

- **Smart compaction in the prune layer (summarization, not truncation).**
  Rejected for pruning. Anything that calls the model belongs to the compaction
  layer (ADR-0021's boundary). Pruning's value is that it is free and instant;
  model-backed relevance ranking would erase that. Tiered truncation is the
  middle ground that reclaims space without spending a call.

- **Keep FIFO; just gate it (ADR-0021) and stop.** Rejected as "no compromise".
  The gating fix addressed *when* pruning fires; these four gaps are about *what
  it does when it fires*, and leaving them would keep the cheapest layer
  needlessly lossy.

## Consequences

- **Positive.** Less signal lost per byte reclaimed: stale and one-shot results
  go first; in-play results survive; large outputs degrade gracefully; the model
  knows what it lost. Pressure tracking can absorb provider feedback without a
  rewrite.
- **Heuristic, not exact.** File correlation and keep-alive can misjudge (a tool
  whose arguments don't name a file; a mention that is coincidental). The safety
  valve is that misjudgment only affects *which* results degrade slightly early —
  never correctness, never the `tool_call_id` chain, and never the durable
  archive (originals are always recoverable).
- **Cost.** Planning is O(n) per pass plus an O(n) forward scan per candidate for
  keep-alive. Pruning is rare and already scans the whole history, so this is
  negligible.
- **No schema change.** The placeholder text changes, but it is free-form
  `content`; legacy placeholders are recognized on read, so no session migration.

## References

- ADR-0019 — model-relative thresholds; deferral of real-usage token feedback.
- ADR-0021 — pruning implicit (silent, ~65%-gated) and renamed distinct from
  compaction (`ContextRelief*`).
- `neenee-core/src/pressure.rs` — `prune_tool_results`, `plan_prune`,
  `mentioned_after`.
- [Context pruning](../explanation/agent-design/context-pruning.md) — the
  design reference, updated for this policy.
- [Context compaction](../explanation/agent-design/context-compaction.md#how-pressure-is-measured)
  — the shared token-measurement policy.
