# 0016. Session review over round-counting stall detection

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

ADR-0009 uncapped the agentic loop on purpose: a finite per-turn round cap is
an arbitrary budget that trips a legitimate long refactor just as readily as a
genuinely stuck model, and the compaction backstop plus user interrupt
(`Esc` / `/pursue stop`) are the right shape for keeping an unbounded loop
bounded. ADR-0009 even rejected keeping a "convergence nudge" without a hard
wall, on the grounds that a nudge with no cap is just an interruption.

A later change re-introduced both, scoped to **read-only** rounds: a hidden
reflection nudge at 8 consecutive read-only tool rounds and a hard abort at
14 (`STALL_THRESHOLD = 8`, `STALL_HARD_STOP_DELTA = 6` in
`crates/neenee-agent/src/lib.rs`, `Agent::update_stall_state`). It walked
ADR-0009 back in two ways at once:

1. **It re-introduced an arbitrary round ceiling.** The hard abort at 14 is a
   finite per-turn cap in everything but name — exactly the shape ADR-0009
   rejected as "safety theatre".
2. **Its signal was a poor proxy for "stuck".** "No write tool fired" is not
   "stuck": a model methodically reading its way through a large codebase
   before a multi-file refactor is *correctly* read-only for many rounds. The
   detector conflated legitimate exploration with looping, and the pain was
   concentrated where it was most wrong — on read-only research **sub-agents**
   (`task`, `verify_plan_execution`), whose every round is read-only by
   profile, so they tripped the 8-round line almost by design.

`docs/explanation/agent-design/harness.md`'s "Safety bounds" section still
advertised distinct tool calls as "uncapped" and never listed the detector,
confirming it was a bolt-on that the documentation had not reconciled.

## Decision

1. **Replace the round-counting stall detector with a periodic session-review
   diagnostic.** Delete `update_stall_state`, `stall_hard_stop_error`,
   `STALL_THRESHOLD`, `STALL_HARD_STOP_DELTA`, the `StallWarning` event, the
   read-only-round bookkeeping in `TurnState`, and the `PRODUCTIVE_READ_TOOLS`
   / `call_was_productive` / `round_was_productive` productivity tracking.

2. **Drive review by total round count, non-terminally.** After
   `review_start_round` (default 64) tool rounds in a turn, and every
   `review_interval_rounds` (default 16) thereafter, the harness spawns a
   bounded read-only diagnostic sub-agent that *reads the live transcript* and
   returns a verdict per registered review dimension. Review never aborts the
   turn; it only surfaces a visible alert and, on an explicit `Stuck` verdict,
   pushes a one-shot reflection nudge so the model gets a chance to recover.

3. **Make the hard stop opt-in.** `hard_stop_rounds` (default `0` = off) is the
   sole execution cap. The default matches ADR-0009 exactly: uncapped, with
   compaction + user interrupt as the backstop. A finite value is an explicit,
   user-declared budget only.

4. **Make review dimensions pluggable.** A `SessionReview` trait
   (`crates/neenee-core/src/session_review.rs`) expresses a dimension as a
   prompt fragment; the runner asks one diagnostic sub-agent to verdict every
   registered dimension, so adding a dimension costs no extra model call.
   `LoopingReview` ("is the agent stuck in an exploration loop?") is the first
   registered dimension. Future dimensions (context bloat, tool-error storms,
   plan drift, …) slot in by implementing the trait.

5. **Disable review on sub-agents.** `TaskTool` and the diagnostic itself seed
   `ReviewConfig::disabled()` (`review_start_round = 0`), so short-lived
   read-only sub-agents never pay for a diagnostic and review can never
   recurse. This also fixes the latent bug where the old detector mis-fired on
   research sub-agents.

6. **A dedicated `REVIEW` sub-agent profile** (`crates/neenee-core/src/subagent.rs`)
   frames the diagnostic as a transcript auditor distinct from `EXPLORE`
   (research) and `VERIFY` (plan auditing).

## Alternatives considered

- **Keep the read-only detector, raise the numbers.** Rejected for the same
  reason ADR-0009 rejected "raise the caps": any finite read-only threshold is
  arbitrary, and "no write fired" is still the wrong signal for a research
  turn. Raising 8 → 64 just moves the cliff.

- **Keep the cheap `StallWarning` signal as a first tier, add the diagnostic
  above it.** Rejected: two overlapping stall mechanisms is exactly the
  two-layer cap system ADR-0009 collapsed. One mechanism (the diagnostic) is
  simpler, and its first invocation at round 64 is late enough that the
  absence of an earlier signal is consistent with ADR-0009's "the activity bar
  shows `turn N · round M` live and the user can interrupt at any time".

- **Single non-streaming `provider.chat()` for the verdict instead of a full
  sub-agent.** Rejected: a sub-agent can open files to verify a looping claim
  (e.g. "is it re-reading the same path?"), reuses the existing `Agent` +
  profile machinery alongside `TaskTool`/`VERIFY`, and is what the diagnostic
  role calls for. It is bounded by `ReviewConfig::for_reviewer()` so it cannot
  loop.

- **Per-dimension sub-agent calls.** Rejected: one diagnostic call that
  verdicts every dimension at once keeps token cost flat as dimensions are
  added.

## Consequences

Positive:

- The default posture is uncapped again (ADR-0009), with a *smarter* signal
  than a round counter: a diagnostic that distinguishes a genuine loop from
  productive exploration.
- Research sub-agents no longer trip a stall line they could never satisfy.
- Adding a health dimension is a trait impl + registration; no dispatch edits.
- The hard stop, when wanted, is an explicit user budget rather than a hidden
  heuristic.

Negative:

- Before round 64 there is no stall signal at all. Accepted: this is the
  ADR-0009 tradeoff ("a genuinely stuck model can run longer before the user
  notices"), mitigated by the live activity bar and `Esc`. The diagnostic at
  64+ is strictly better than nothing and far cheaper than a hard cap.
- Each review run is one extra model inference. Bounded by the 16-round
  interval and the 8k-char transcript excerpt handed to the reviewer.

Migration:

- `[agent] stall_threshold` is removed from `config.toml`. Replace it with the
  `[agent.review]` table (`review_start_round`, `review_interval_rounds`,
  `hard_stop_rounds`). A config with the old key is silently ignored (serde
  `default`), behaving as the new default.
- The `/stall-threshold` slash command is replaced by `/review`
  (`/review`, `/review off`, `/review N [M]`, `/review default`).
- `AgentEvent::StallWarning` / `AgentResponse::StallWarning` become
  `SessionReview { alert }`; the TUI's `stall_rounds` counter becomes a
  `review_alert: String`.

## References

- [ADR-0009](0009-uncapped-agentic-loop.md) — the uncapped loop this restores
  and extends.
- [ADR-0011](0011-subagent-profiles.md) — the profile primitive the `REVIEW`
  profile extends.
- [ADR-0012](0012-toolaccess-tier-split.md) — the `ToolAccess` tiers the
  `REVIEW` profile's read-only ceiling is built on.
- `crates/neenee-core/src/session_review.rs` — `SessionReview` trait,
  `ReviewConfig`, `ReviewVerdict`.
- `crates/neenee-core/src/subagent.rs` — the `REVIEW` profile.
- `crates/neenee-agent/src/session_review.rs` — the diagnostic runner +
  `LoopingReview`.
- `crates/neenee-agent/src/agent.rs` — `Agent::update_review_state`,
  `Agent::run_session_review`.
- [Harness architecture](../explanation/agent-design/harness.md) — the safety
  surface, updated.
