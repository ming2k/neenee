# 0030. Early loop intervention (in-loop semantic review + anti-anchoring nudge) and a constrained round-count hook

- **Status:** Accepted
- **Date:** 2026-06-25

## Context

A model can get stuck repeating the same or near-identical read-only tool calls
(`read_file` of adjacent line ranges, `grep` with minor argument tweaks) without
making progress — a self-reinforcing trajectory the agent loop has no internal
reason to exit. Three gaps in the current harness let it run too long:

1. **No early signal.** ADR-0016 replaced the round-counting stall detector with
   a semantic diagnostic (`LoopingReview`), then ADR-0018 made that diagnostic
   on-demand (`/review` → `Agent::review_now`, `agent.rs:1943`). The turn loop
   no longer fires any review itself (`agent.rs:1886`), so before the user
   manually asks, the *only* automatic backstop is `guard_repeated_call`
   (`agent.rs:1853`, `MAX_REPEATED_TOOL_CALLS = 3` in `lib.rs:92`).

2. **The backstop sees exact equality only.** `guard_repeated_call` keys on
   `(name, arguments)` string equality. Micro-adjusted re-reads (offset/limit
   nudged by a line or two) never compare equal, so they accrue no count and
   bypass the guard entirely — the failure mode seen in the field. The guard
   only trips once arguments happen to collide, arbitrarily late.

3. **No intervention, only a hard stop.** When the guard does fire it aborts the
   turn with an error. There is no in-band steering that breaks the
   self-reinforcing trajectory while the turn keeps running — no anti-anchoring
   nudge.

ADR-0025 deliberately kept hooks on a single lifecycle-event axis and excluded
round-count/threshold/clock as hook axes (they are owned by `CompactionPolicy`,
`/pursue`, `/repeat`). That decision is reassessed below: its strongest stated
reason ("overlaps the existing engines") does not hold for "run *user* logic at
round N", and its deepest worry ("reintroduces a round cap") applies equally to
the `Stop` hook it already accepts and handles by composition.

## Decision

Three changes, staged so each ships independently.

### 1. A built-in steering module for nudges (`neenee-agent/src/steering.rs`)

Nudges already exist as one-shot, bespoke code paths — `should_nudge_verify` /
`should_nudge_todos` (`agent.rs:1971`/`:1988`) with mirrored injection blocks in
both the streaming and non-streaming loops (`agent.rs:1284`/`:1555` and
`:1300`/`:1575`), each with its own `TurnState` gate (`verify_nudged`,
`todo_nudges`). The shape is identical — *condition on turn state → one-shot
gate → inject a hidden user message* — and a fourth nudge (the loop
anti-anchoring nudge below) makes the abstraction earn its keep (the YAGNI bar
ADR-0025 applied when deleting the one-shot traits).

Add `steering.rs` exporting a `Nudge` abstraction: a condition over `&TurnState`
(and read-only `&Agent`), a per-turn latch, and a prompt builder. The new loop
nudge lives here. **Existing verify/todo nudges are not force-migrated**; that is
a later, mechanical refactor deferred to avoid behaviour risk in this change.
The module stays out of the hooks bus on purpose — like `SessionReview`
(`session_review.rs`) and `ContextReliefGate`, it is harness-internal steering,
not a user-configurable interception point.

### 2. Early loop intervention: in-loop semantic review + anti-anchoring nudge

Drive `Agent::review_now` from inside the turn loop, early and non-terminally,
on a **weak round-level signal**, and act on a `Stuck` verdict by injecting the
anti-anchoring nudge rather than aborting.

- `TurnState` gains `consecutive_readonly_rounds: u32` and a one-shot
  `loop_review_fired: bool`.
- `dispatch_tool_calls` (`agent.rs:1627`) classifies the round's tool calls by
  `ToolAccess` (looked up via `self.tools`): a round with at least one
  `Execute`/`Write` call resets `consecutive_readonly_rounds` to 0; an all-`Read`
  round increments it. This is a **trigger** only, not a productivity verdict —
  the verdict still comes from `LoopingReview`, which is exactly the split
  ADR-0016 settled on (deterministic trigger, semantic judgement).
- At each round boundary (after `tool_rounds += 1`, mirrored in both loops), if
  `!loop_review_fired && (consecutive_readonly_rounds >= LOOP_REVIEW_ROUNDS ||
  repeated_calls >= LOOP_REVIEW_REPEATED)` then set the latch and run
  `review_now(messages)`. Constants in `lib.rs`: `LOOP_REVIEW_ROUNDS = 6`,
  `LOOP_REVIEW_REPEATED = 2`. Either arm alone is too blind: read-only-round
  count catches micro-adjusted re-reads the equality guard misses; the equality
  counter catches tight loops that happen to interleave a non-read call.
- On any `Stuck`/`Degraded` verdict, inject the `LoopingNudge` (a hidden user
  message that names what was repeated, forbids re-reading it, and demands a
  forward action). The turn continues; the user keeps `Esc` and the opt-in
  `hard_stop_rounds` (ADR-0016) as the hard backstop.

This fills the "no early signal" and "no intervention" gaps without resurrecting
the arbitrary hard abort ADR-0016 rejected: the trigger is round-shaped but the
decision is semantic and non-terminating.

### 3. A constrained round-count hook (partially supersedes ADR-0025's exclusion)

Add one new hook event, tightly constrained:

- `HookEventKind::Round` and `HookEvent::Round { round, consecutive_readonly }`
  in `neenee-core/src/hooks.rs`.
- **`Deny` is ignored on `Round`** (a round-count hook may not become a de-facto
  round cap — the ADR-0009 concern). Only `Inject`/`Pass` are honoured; a
  returned `Inject` becomes a hidden user message on the next round, exactly like
  `PostToolUse`.
- One funnel: the round boundary beside the review trigger from Decision 2, so
  the same site serves both built-in steering and user hooks.
- Config: `event = "Round"` in the existing `[[hooks]]` table; no new config
  shape.

The harness declares **no built-in threshold** on this axis (it does not fire its
own `Round` hook); it only provides the trigger point. Users opt in to round
driven logic themselves, which is precisely how ADR-0016's "arbitrary threshold,
who decides?" worry is answered — the user decides, at their own risk, and the
`Deny`-forbidden constraint keeps them from silently recreating a cap.

## Alternatives considered

- **Resurrect the ADR-0016 round-counting stall detector** (hidden reflection
  nudge at N read-only rounds, hard abort at N+Δ). Rejected for the reasons
  ADR-0016 gave: a hard abort is a finite cap in disguise, and "no write fired"
  mis-fires on legitimate research turns and on read-only sub-agents. Decision 2
  keeps the deterministic trigger but moves the verdict to `LoopingReview` and
  never aborts.

- **Route the loop nudge through the hooks bus.** Rejected (and ADR-0025 already
  settled this): hooks are user-configurable, default-empty, and cannot read
  `TurnState`. Built-in steering needs internal state and must work with no user
  config. The steering module and the hooks bus stay separate, as
  `SessionReview` already does.

- **Widen `guard_repeated_call` to "similar" calls instead of adding review.**
  Rejected as the primary fix: normalising `read_file` arguments to ignore
  offset/limit catches the one micro-read pattern but is brittle (every
  read-heavy tool needs its own normaliser) and still only *aborts*; it produces
  no in-band course correction. It is complementary — a normalised signature can
  raise the `repeated_calls` counter sooner — but it does not replace semantic
  review. Left as a possible follow-up, not part of this ADR.

- **Make round-count a full hook axis with `Deny`.** Rejected: an unconstrained
  round hook is the blanket round cap ADR-0009 removed, just user-configured. The
  `Deny`-forbidden constraint in Decision 3 keeps the useful part (user logic at
  a round boundary) and drops the dangerous part.

## Consequences

Positive:

- Micro-adjusted re-read loops get an in-band, semantic, non-terminating
  intervention well before the equality guard would (or wouldn't) fire.
- `review_now` finally has an automatic trigger inside the turn loop, closing the
  gap ADR-0018 opened by making review purely on-demand.
- The steering module gives every future nudge one home, arresting the
  per-nudge bespoke path that verify/todo/plan-exit started.
- Users gain a round-boundary hook for their own observability/injection logic
  without the harness taking on an arbitrary threshold.

Negative:

- One extra model inference the first time the trigger fires in a turn. Bounded
  by the one-shot latch (at most one automatic review per turn) and the 8k-char
  transcript excerpt the reviewer already uses.
- `dispatch_tool_calls` now touches `ToolAccess` per call. Cheap (a `Vec` scan),
  but it is the first round-productivity bookkeeping since ADR-0016 removed it —
  kept strictly as a *trigger*, never as a verdict, to avoid the conflation
  ADR-0016 warned against.

Neutral:

- ADR-0025's "round-count is not a hook axis" is **partially superseded**: a
  single, `Deny`-forbidden `Round` event is added. The rest of ADR-0025's axis
  discipline (no `NearCompact`, no generic `Every`/`Interval`) stands.

Migration (staged, each shippable):

1. **Stage 1 (with this ADR):** `steering.rs` + the `LoopingNudge`; `TurnState`
   read-only-round tracking in `dispatch_tool_calls`; the round-boundary review
   trigger + nudge injection in both loops; constants in `lib.rs`; tests.
2. **Stage 2:** the constrained `Round` hook — `HookEventKind`/`HookEvent` in
   `neenee-core`, the `run_round` query in `HookRegistry`, the round-boundary
   funnel, `event = "Round"` in config + the CLI command runner.
3. **Stage 3 (optional, later):** migrate `should_nudge_verify` /
   `should_nudge_todos` onto the `steering.rs` `Nudge` abstraction and delete the
   bespoke injection blocks.

## References

- [ADR-0009](0009-uncapped-agentic-loop.md) — the uncapped loop; the `Round`
  hook's `Deny`-forbidden constraint honours its "no blanket round cap" stance.
- [ADR-0016](0016-session-review-over-round-counting.md) — semantic review over
  round counting; this ADR gives that review an automatic early trigger.
- [ADR-0018](0018-per-project-multi-instance-concurrency.md) — made review
  on-demand, opening the "no early signal" gap this ADR closes.
- [ADR-0025](0025-lifecycle-event-hooks.md) — single-axis hooks; this ADR
  partially supersedes its exclusion of round-count by adding one constrained
  event.
- [ADR-0026](0026-plan-progression-forcing-functions.md) — the bespoke nudge
  pattern the `steering.rs` module generalises.
- `crates/neenee-agent/src/agent.rs` — `guard_repeated_call` (`:1853`),
  `dispatch_tool_calls` (`:1627`), `review_now` (`:1943`), round boundaries
  (`:1274`, `:1540`).
- `crates/neenee-agent/src/session_review.rs` — `LoopingReview`, the verdict
  producer this ADR wires into the loop.
