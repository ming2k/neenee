# 0044. Layered token accounting: upstream-reported usage with char-class estimation fallback

- **Status:** Accepted
- **Date:** 2026-01-15

## Context

Context pressure — the token count compared against a model's context window to
trigger [pruning](../explanation/agent-design/context-pruning.md) and
[compaction](../explanation/agent-design/context-compaction.md) — was, until
this decision, computed entirely by a local heuristic: `estimate_tokens` divided
the UTF-8 byte length of the message list by `CHARS_PER_TOKEN = 4`
(`crates/neenee-core/src/pressure.rs`).

Two problems motivated a change:

1. **The `bytes / 4` heuristic is wrong for the text neenee actually carries.**
   CJK text is severely under-counted (a Chinese ideograph is ~1 token but ~3
   UTF-8 bytes, so four characters estimate as `12/4 = 3` instead of ≈4, and
   longer CJK stretches under-count by 3–4×). Source code is unevenly counted
   — brackets/operators that BPE splits into single tokens are denser than `4`
   predicts. neenee's conversations are overwhelmingly CJK + code, so the
   pressure meter systematically misled: it read low when the window was near
   full (risking silent provider-side truncation) and triggered compaction at
   the wrong time.

2. **Provider-reported usage was never used.** An earlier effort (ADR-0019 /
   ADR-0023, "layered token accounting") designed a `usage`-preferring pressure
   policy, but the `Provider` trait never exposed usage, so the policy function
   (`effective_pressure_tokens` / `USAGE_TRUST_FLOOR`) had no production caller
   and was removed as dead code. Each provider adapter (Anthropic, OpenAI-compat,
   Gemini) parsed the HTTP response's `content` but **discarded** the `usage`
   object — Anthropic's `message_delta.usage` even fell through a `_ =>`
   wildcard. So 100% of token counts were estimated, and `TokenUsage.prompt_tokens`
   was effectively always zero.

The full design is described in
[Token accounting](../explanation/agent-design/token-accounting.md).

## Decision

Adopt a **layered** token-accounting policy with two sources, a single booking
point, and observable attribution:

1. **Upstream-reported usage is authoritative when present.** Extend the
   `Provider` trait with two default methods — `usage_supported() -> bool` and
   `take_last_usage() -> Option<TokenUsage>` (a consume-once drain) — and add a
   `ProviderStreamEvent::Usage(TokenUsage)` variant for the streaming path. Have
   each real adapter parse the `usage` object it already receives and implement
   these.

2. **Fall back to a char-class estimator when upstream is absent.** Replace
   `bytes / 4` with `count_tokens`, which classifies each Unicode scalar into a
   category (ASCII word / CJK glyph / code punctuation / …) and weights it
   fractionally (CJK ≈ 1.0 token/char, ASCII words ≈ 0.25, etc.) using
   fixed-point integer math. Single O(n) pass, no external vocabulary.

3. **Book every turn through one point.** `Agent::book_turn_usage` resolves a
   turn's usage via the chain `streamed Usage event → take_last_usage() →
   estimate_message_tokens`, adds it to the turn's `TokenUsage`, and records it
   into a shared `TokenSourceLedger` tagged *reported* or *estimated*.

4. **Surface the attribution.** A new `TokenSourceLedger` (per
   `provider × model`) accumulates reported-vs-estimated totals; the TUI's
   hint-bar context meter becomes clickable, opening a read-only **Token Source
   Report** modal showing per-model reported/estimated counts and a `% Real`
   accuracy figure.

## Alternatives considered

- **Bundle a real tokenizer (tiktoken / the model's BPE).** Rejected: each model
  family has its own tokenizer, bundling several multi-MB vocabularies bloats
  the binary, and the provider's *own* count (returned in `usage`) is always
  more accurate than any client-side tokenizer because it accounts for the
  chat-template framing overhead the client cannot see. The char-class estimator
  is "good enough" precisely because the authoritative path exists for the
   providers that matter.

- **Revive ADR-0019/0023 verbatim.** Rejected as written: that design coupled
   pressure policy to a `USAGE_TRUST_FLOOR` blend of reported + estimated in one
   number, which obscures which counts are authoritative. This decision keeps
   the two sources *separate and attributed* rather than blended, so the meter
   and the report can show accuracy rather than hide it.

- **Change `CHARS_PER_TOKEN` for the reverse (token→character) conversions too.**
   Rejected: `summary_char_budget` and `prune_protect_chars` use the flat ratio
   as a deliberately conservative character over-estimate; the char-class model
   is not better in that direction, and changing the constant would shift every
   compaction threshold. The constant is kept as a one-way conversion factor.

## Consequences

**Positive.** CJK-heavy and code-heavy conversations get a roughly correct
pressure meter for the first time (4 Chinese chars estimate as 4 tokens, not
1). Providers that return `usage` contribute fully authoritative counts. Users
can see, per model, whether their meter is measured or guessed, and act on it
(e.g. switch off a relay that strips usage, or know to distrust the percentage
for an estimated model).

**Negative.** The estimator is still a heuristic: framing overhead (chat
template, system-prompt tokenization) remains unaccounted for on the estimated
path, so estimated counts run low by a roughly constant offset. The char-class
weights are calibrated by category averages, not measured per-model, so they
will not match any single tokenizer exactly. The Token Source report can show
`0%` for a model that is in fact fully capable of reporting usage but whose
relay strips the field — the report is honest about that, but a user must
understand the distinction.

**Neutral.** The `Provider` trait gains two more default methods; existing test
doubles and any out-of-tree provider are unaffected (defaults return
`false` / `None`). The `ProviderStreamEvent` enum gains a variant; exhaustive
`match` sites were updated.

## References

- Supersedes the intent of the deleted ADR-0019 / ADR-0023 "layered token
  accounting" (whose `effective_pressure_tokens` / `USAGE_TRUST_FLOOR` were
  removed as dead code — see the note at
  `crates/neenee-core/src/pressure.rs` `estimate_tokens`).
- [Token accounting](../explanation/agent-design/token-accounting.md) — full
  design reference.
- [Context compaction](../explanation/agent-design/context-compaction.md) and
  [Context pruning](../explanation/agent-design/context-pruning.md) — the layers
  that consume the token count.
