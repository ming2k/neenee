# 0019. Model-relative context compaction

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

Context compaction (the backstop that keeps the uncapped agentic loop from
exhausting the context window, ADR-0009) triggered on a single fixed character
budget: `compaction_max_chars` defaulted to `120_000` (~30k tokens). Pressure
was measured everywhere with `estimate_chars`, and the mid-turn prune threshold
hard-coded that budget at `3/4`.

The model registry (`neenee-core/src/model.rs`) already records each model's
context window in tokens — from 128k (`gpt-4o`) through 262k (`kimi-k2.7-code`)
to 1M (`glm-5.2`, Gemini, DeepSeek) — and the TUI already rendered a
pressure meter from it. But the compaction thresholds never read it, so the
effective policy was model-agnostic and broke down at the extremes:

- On a 1M-token model, compaction fired at ~3% of the window — absurdly early,
  over-summarizing and wasting the large context the user chose the model for.
- On a 128k model, ~23% was merely coincidental, not designed.

Two more problems followed from the frozen budget:

- `/provider` could switch to a model with a different window mid-session, but
  the mid-turn threshold was seeded once at startup and never re-seeded, so
  relief tracked the *first* model, not the live one.
- Pressure was measured in characters while the window is denominated in
  tokens, so "fraction of the window" was an estimate-of-an-estimate.

## Decision

Make compaction thresholds a function of the **active model's context window**,
denominated in **tokens**:

1. A declarative `CompactionPolicy` (`neenee-core/src/pressure.rs`) expresses
   the thresholds as fractions of the window plus a conservative fallback
   window for models the registry does not know:
   - `utilization` (0.85) — trigger a full summarizing compaction.
   - `prune_utilization` (0.65) — trigger cheap tool-result pruning (below the
     full-compaction threshold).
   - `target_utilization` (0.25) — compress the active window down to this
     fraction after a full compaction.
   - `fallback_window_tokens` (32_000) — assumed window when the resolved
     window is `0` (unknown/local models), so relief still engages.

2. `CompactionPolicy::resolve(window_tokens)` produces a `ContextBudget` of
   absolute token thresholds. Pressure is measured with `estimate_tokens` and
   compared directly against these.

3. The runtime `CompactionSettings` (in `neenee-agent`) carries the resolved
   `ContextBudget` and is built per turn from the **live** model via
   `agent.provider.model()`. The mid-turn prune threshold is re-seeded on
   startup *and* on every provider/model switch (`SwitchProvider`,
   `SetDefaultModel`), so relief always tracks the current model.

4. The escalation ladder from ADR-0009 is preserved — cheap pruning first, full
   summarizing compaction second, reactive overflow recovery last — only the
   *threshold derivation* changes.

## Alternatives considered

- **Keep the fixed `compaction_max_chars` budget.** Rejected: it is wrong for
  every model except the one it happened to suit, and it is blind to provider
  switches. The model registry already holds the authoritative window.

- **Wait for real `prompt_tokens` from the provider before changing anything.**
  Rejected as a prerequisite: the `Provider` trait does not surface token
  usage today, and threading it through every provider implementation and SSE
  parser is a separate epic. `estimate_tokens` (~4 chars/token) is a
  dimensionally-correct stand-in — it shares the window's token unit — so the
  threshold model is correct now. The `ContextBudget` shape is unchanged by a
  future switch to real usage; only the pressure measurement at the call site
  would change.

- **A single "85% → 20%" trigger with no escalation ladder.** Rejected: the
  loss-tolerant pruning layer (keeps the `tool_call_id` chain intact) is cheap
  and safe to run earlier and more often than a full compaction. Collapsing
  the ladder would either prune too aggressively or compact too often.

## Consequences

- **Positive.** Compaction now uses the context the model actually has: a 1M
  model is no longer over-compacted at 3%; a 128k model is no longer
  under-protected. Relief follows `/provider` switches immediately.

- **Config compatibility.** The flat keys `compaction_max_chars` and
  `compaction_prune_protect_chars` are removed; a `[compaction]` table
  (`utilization`, `target_utilization`, `prune_utilization`,
  `fallback_window_tokens`) replaces the first, and
  `compaction_prune_protect_tokens` (6_000) replaces the second. Existing
  config files parse unchanged (serde ignores unknown keys); the removed
  values are silently dropped in favor of the model-relative defaults.

- **Neutral.** Pressure is now estimated in tokens rather than characters;
  `CompactionCheckpoint` still reports `before_chars`/`after_chars` for display
  continuity.

## References

- ADR-0009 — uncapped agentic loop; compaction as the runaway backstop.
- `neenee-core/src/pressure.rs` — `CompactionPolicy`, `ContextBudget`.
- `docs/explanation/agent-design/harness.md` — the three-layer compaction
  description.
