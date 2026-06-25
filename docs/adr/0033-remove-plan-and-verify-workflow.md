# 0033. Remove the plan-as-subagent and verify workflow

- **Status:** Accepted
- **Date:** 2026-06-25

## Context

ADR-0026 added three turn-exit forcing functions (plan-exit nudge,
todo-continuation nudge, verify-nudge) and ADR-0027 replaced Plan mode with a
`PLAN` subagent profile, a `plan` tool, and a `verify_plan_execution` tool
(keeping ADR-0012's `VERIFY` profile as the verifier's role). The goals were
real: drive a plan to completion, force an independent audit before a turn
ends, and keep planning out of the main transcript.

In practice the machinery never justified its weight:

- The forcing cascade (plan-exit → todo-continuation → verify-nudge → pursue
  stop-gate) was the single most complex part of the turn loop, duplicated
  across the streaming and non-streaming paths in `agent.rs`.
- The `plan` / `verify_plan_execution` tools and the `PLAN` / `VERIFY`
  profiles were the only consumers of the `write_paths` grant
  (ADR-0028) and of the `Execute` ceiling (ADR-0012). With them gone the
  profile vocabulary collapses to `Read`-only roles.
- The todo-continuation and verify nudges fought the model: they re-injected
  hidden messages that frequently produced brittle, repetitive behaviour
  rather than honest progress.
- The `MAX_REPEATED_TOOL_CALLS` hard-abort guard (3 identical calls) was
  coupled to this path; ADR-0030's early, non-terminating in-loop
  intervention is a strictly better replacement.

The removal landed in commit `a6356d5` ("drop plan/verify workflow"),
deleting `plan_subagent.rs`, `plan_verify.rs`, the `plan_enter` / `plan_exit`
/ plan-seed tools, the `VERIFY` profile, the three nudges, and
`MAX_REPEATED_TOOL_CALLS`. This ADR records that decision so the supersede
chain is honest; without it ADR-0026 and ADR-0027 still read as binding.

## Decision

Remove the plan-as-subagent and verify workflow in full.

1. **Delete the tools.** Remove the `plan`, `verify_plan_execution`,
   `plan_enter`, and `plan_exit` tools. Planning is now a prompt-level
   activity the model does with its normal read/write tools; there is no
   dedicated plan tool, no plan file under `.neenee/plans/`, and no plan-state
   seeding of the todo list.
2. **Delete the `VERIFY` and `PLAN` subagent profiles.** The built-in
   profiles are now `EXPLORE` (read-only research), `REVIEW` (read-only
   session diagnostic), `TITLE` (read-only title generation), and
   `INTERACTIVE` (opt-in user interaction).
3. **Delete the turn-exit forcing cascade.** The only turn-end gate is
   `stop_gate` (`agent.rs`): the `/pursue` stop-gate (ADR-0015) combined with
   any `Stop` hooks (ADR-0025). There is no plan-exit nudge, no
   todo-continuation nudge, and no verify-nudge. An ordinary turn (no pursuit,
   no denying/continuing hook) ends when the model stops calling tools.
4. **Drop the `MAX_REPEATED_TOOL_CALLS` hard abort.** Early in-loop
   intervention (ADR-0030) handles unproductive looping without a hard abort.
5. **Keep the unified todo list (ADR-0020).** The `todo` / `todo_update`
   tools and the Activity-modal task list survive — they are no longer seeded
   by a plan, but the model still uses them to track multi-step work.

## Alternatives considered

- **Keep `verify_plan_execution` as a standalone verifier subagent.**
  Rejected: its only reason to exist was the forcing gate that drove it.
  A model that wants an independent audit can spawn a read-only `subagent`
  directly.
- **Keep the `VERIFY` profile for general command-running subagents.**
  Rejected: there is no dispatch tool that binds it, and admitting `bash` into
  a subagent is better expressed by a future profile when a concrete need
  appears, not carried as dead vocabulary.
- **Rewrite ADR-0026 / ADR-0027 in place to reflect the removal.** Rejected:
  ADRs are immutable. The supersede chain is the correct record.

## Consequences

- **Positive.** The turn loop becomes the model + the pursue stop-gate + hooks.
  The harness is materially simpler; the streaming and non-streaming paths no
  longer carry a forcing cascade. The profile vocabulary matches reality.
- **Positive.** `.neenee/plans/` and `active_plan_path` are gone, so paths,
  the TUI, and the system-prompt builder all shed a special case.
- **Negative.** The model no longer gets an automated "verify before
  completion" push; callers that relied on it should ask for verification
  explicitly or arm a `/pursue` with a completion condition.
- **Migration.** The `agent.verify_nudge_enabled` and
  `agent.loop_review_enabled` config keys still parse but do nothing (kept
  for config compatibility; `loop_review_enabled`'s live behaviour moved to
  ADR-0030's in-loop review). The `/verify-nudge` slash command is removed.

## References

- Supersedes [ADR-0026](0026-plan-progression-forcing-functions.md) (the
  forcing functions) and [ADR-0027](0027-plan-as-subagent.md) (the `PLAN`
  profile and `plan` tool).
- Narrows [ADR-0012](0012-toolaccess-tier-split.md): the `VERIFY` profile is
  removed; the `Read < Execute < Write` tier split stays but only the main
  agent and `Read`-ceiling subagents remain.
- [ADR-0028](0028-capability-allocation-scoped-writes.md) — the
  `WriteScope` / `write_paths` mechanism survives; no built-in profile uses
  it today.
- [ADR-0030](0030-early-loop-intervention-and-round-hook.md) — the
  non-terminating in-loop intervention that replaces the hard-abort guard.
- Commit `a6356d5` — the removal.
