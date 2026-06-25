# 0034. Range-aware prune staleness and a deterministic read-loop guard

- **Status:** Accepted
- **Date:** 2026-06-26

## Context

A model occasionally got stuck re-reading one file without making progress — a
read loop that the harness never broke on its own, so it spun until the user
hit `Esc`. Two independent defects compounded into this failure:

1. **Pruning evicted live pages.** `prune_tool_results`
   (`neenee-core/src/pressure.rs`) keyed staleness on the *path only*
   (`file_key`, ADR-0023): "the last tool result for a given file is live;
   earlier ones touching the same file are stale once it is re-touched." That
   conflates two unrelated relationships. A `write`/`edit` genuinely invalidates
   prior reads, but two reads of *different pages* of one file are
   **complementary, not superseding**. Once context crossed the 65% prune
   threshold, reading page B marked page A stale and cleared it to a
   `[cleared tool result: …]` placeholder — whose explicit purpose is to invite
   re-fetch. The model, having lost page A, re-read it, which then evicted page
   B: a `A B A B` oscillation driven by the pruner itself.

2. **No automatic loop intervention.** ADR-0016 replaced round-counting with an
   on-demand semantic review; ADR-0018 made it `/review`-only; ADR-0030 added an
   automatic in-loop review + anti-anchoring nudge, which was then removed (with
   the `guard_repeated_call` equality guard) in favour of the model-initiated
   `abort` tool. The result: in the default configuration nothing automatically
   breaks a read loop. `hard_stop_rounds` defaults to `0` (uncapped, ADR-0009),
   the `Round` hook is opt-in and empty by default, `/review` is manual, and
   `abort` requires the model to *notice* it is stuck — which a looping model, by
   definition, does not.

Defect 1 *causes* genuine re-reads under pressure; defect 2 is why a loop, once
started (for any reason), never ends on its own.

## Decision

Two changes, addressing cause and escape independently.

### 1. Range-aware prune staleness (`pressure.rs`)

`ToolMeta` carries `read_range: Option<(usize, usize)>` (the 1-based
`[offset, offset+limit)` a read covered; `usize::MAX` end = open-ended) and
`mutates: bool`, classified from the call's argument *shape*
(`classify_file_touch`): `content`/`new_string`/`old_string` ⇒ mutation;
otherwise a read with normalized pagination. A later same-file result
`supersedes` an earlier one only when:

- it is a **mutation** (any prior read of that path is now outdated), or
- it is a **read that fully covers** the earlier read's line range
  (`range_covers`) — a genuine re-read / superset.

Reads of different pages never supersede each other, so paging no longer
self-evicts. The `write`/`edit` invalidation path (ADR-0023) is preserved
exactly.

### 2. A deterministic read-loop guard (`neenee-agent/src/loop_guard.rs`)

A self-contained module that detects repeated reads and injects a
non-terminating anti-anchoring nudge:

- **Signal.** Identical read arguments return byte-for-byte identical content,
  so a repeated read is a *provable* waste — no LLM adjudication needed (unlike
  the fuzzier "many distinct reads" case `LoopingReview` handles). Detection is
  pure signature bookkeeping: free, instant, zero false positives on legitimate
  work (which reads *different* things).
- **Frequency window, not a consecutive counter.** A "same N rounds in a row"
  counter misses the `A B A B` thrash above. The guard keeps a sliding window of
  the last `WINDOW = 8` read-round signatures and fires when any signature
  occurs `THRESHOLD = 3` times *within* it — catching `A A A`, `A B A B A`, and
  leaving genuine forward paging (`A B C D E`, all distinct) untouched.
- **Signature.** A file read is keyed on `name|path|offset|limit` with
  pagination defaults normalized (so toggling a default cannot dodge the guard,
  and a different range is a different signature). A query-shaped read (`grep`)
  falls back to raw arguments so distinct queries stay distinct.
- **Action.** A hidden user message (`InjectionKind::LoopReviewNudge`) appended
  at the round boundary, naming the repeated read and demanding a different
  action. A one-shot-per-signature latch prevents spam and escalates once to
  sterner wording at `ESCALATE_AT = 6`. The turn is **never** terminated —
  `Esc`, `hard_stop_rounds`, and `abort` remain the hard backstops (ADR-0009).
- **Gating.** Revives `Agent::set_loop_review_enabled` (a dead no-op since
  ADR-0030's removal) as the real off-switch, seeded from
  `[agent] loop_review_enabled` (default `true`) and flipped off for sub-agents
  and the `/review` diagnostic. Detection has no recursion risk (no model call),
  so the flag is a convenience, not a safety requirement.

Wired at both loop boundaries in `Agent::{run_streaming_with_events,
run_with_events}` via `maybe_inject_loop_nudge`, beside `relieve_pressure_if_needed`
and `run_round_hooks`. `dispatch_tool_calls` classifies each round into
`TurnState::pending_round`, reusing the existing read-only classification.

## Alternatives considered

- **Normalize `file_key` to ignore offset/limit (path-only re-read dedup).**
  Rejected: it would make legitimate paging (`A B C`) collapse to one signature
  and punish forward progress — the opposite of the fix. Range-awareness is what
  distinguishes a re-read from a new page.

- **Resurrect `guard_repeated_call`'s hard abort.** Rejected as ADR-0016/0009
  did: a hard stop is a round cap in disguise and mis-fires on legitimate
  research. The guard steers (a nudge) instead of capping.

- **Re-instate ADR-0030's semantic review trigger.** Rejected as the *primary*
  mechanism: running `review_now` per trigger costs a model inference, and for
  the specific "identical repeated read" symptom the waste is provable without
  one. Semantic review stays available on-demand (`/review`) for the fuzzier
  case; the deterministic guard handles the tight loop cheaply.

- **A consecutive-identical counter.** Rejected: blind to the `A B A B`
  oscillation that the prune defect (and two-region work) produces. The
  frequency window subsumes it.

## Consequences

Positive:

- The pruner stops manufacturing re-reads; the guard breaks any read loop that
  starts for any other reason, at any context level — closing both the cause and
  the escape gap in the default configuration.
- Detection is free (no inference, no recursion) and false-positive-free on
  legitimate paging and research.
- `set_loop_review_enabled` / `[agent] loop_review_enabled` is meaningful again
  instead of a dead stub.

Negative / neutral:

- Sub-agents and the review diagnostic do not get the nudge (flag off), matching
  prior wiring; a looping sub-agent still relies on its parent and `abort`.
  Revisit if sub-agent read loops appear.
- `dispatch_tool_calls` now computes a round signature on all-read rounds (a
  cheap `Vec` map + sort), in addition to the existing read-only classification.

## References

- [ADR-0009](0009-uncapped-agentic-loop.md) — uncapped loop; the guard honours
  its "no blanket round cap" stance by steering, not capping.
- [ADR-0016](0016-session-review-over-round-counting.md) — semantic review over
  round counting; the deterministic guard complements, not replaces, it.
- [ADR-0023](0023-relevance-aware-tiered-pruning-and-layered-token-accounting.md)
  — the path-only staleness this ADR makes range-aware; keep-alive and tiered
  degradation are unchanged.
- [ADR-0030](0030-early-loop-intervention-and-round-hook.md) — the removed
  automatic intervention this ADR revives in deterministic form; the `Round`
  hook it added is untouched.
- `crates/neenee-core/src/pressure.rs` — `ToolMeta::supersedes`,
  `classify_file_touch`, `plan_prune`.
- `crates/neenee-agent/src/loop_guard.rs` — the guard; `agent.rs`
  `maybe_inject_loop_nudge` and the round-boundary wiring.
