# 0006. Plan mode v2: approval gate, active plan path, proposed-plan rendering

- **Status:** Accepted
- **Date:** 2026-06-21

## Context

Plan mode shipped in the initial six-crate topology (see ADR-0005). It modeled
the read-only planning surface as a second `AgentMode` and gated tool
execution through `Tool::allowed_in_plan_mode`. Three gaps became obvious
once it was used in practice:

1. **No approval step.** `plan_exit` flipped the mode unconditionally. The
   model could decide on its own that planning was done and start editing,
   which defeated the point of asking for a plan in the first place.
   opencode's `plan_exit` and claude-code's `ExitPlanModeV2Tool` both block
   on an explicit user yes/no; codex routes the same moment through a
   `<proposed_plan>` tag and a TUI popup.
2. **No follow-up.** After the mode flipped back to Build, nothing recorded
   *which* plan was approved. The model had to re-read the plan file every
   turn or silently drift from it. claude-code and opencode both track the
   approved plan path and surface it in later system prompts.
3. **Sparse prompt guidance.** The Plan-mode system prompt was four lines.
   It said "research, write a plan, exit" but did not teach the model what
   a good plan looks like, when to ask the user, or what counts as
   "ready." codex's three-phase template (grounding → intent chat →
   implementation chat) and its "decision-complete" finalization rule are
   noticeably more effective in practice.

The three reference implementations also disagree on presentation: opencode
and claude-code persist the plan to a file and read it back; codex emits a
`<proposed_plan>...</proposed_plan>` tag inline that the TUI renders as a
distinct card. neenee wanted both: a persisted plan file (for resume and
for the Build-mode hint) and a visual signal in the transcript.

## Decision

Plan mode becomes a four-part mechanism, all running through the existing
shared `Arc<Mutex<AgentMode>>` so the manual `/mode` command and the
autonomous tools never disagree.

1. **Approval gate on `plan_exit`.** The agent intercepts `plan_exit` in
   `Agent::execute_tool` (mirroring the existing `ask_user` special case)
   and routes it through `UserQuestionRequest`. The user picks "Approve"
   or "Keep planning". On approval the underlying tool runs (flips the
   mode, reads the plan body, returns it). On rejection the agent stays
   in Plan mode with a message asking what to refine. Manual `/mode
   build` skips the gate — when the user types the slash command they
   have already decided.

2. **`active_plan_path`.** A new `Arc<Mutex<Option<PathBuf>>>` lives next
   to the mode cell, shared between `Agent` and `PlanToolContext`.
   `plan_exit` records the approved path; `plan_enter` and manual `/mode
   plan` clear it. The Build-mode system prompt reads it each turn and
   appends "You are implementing the approved plan at \<path\>" with
   instructions to spawn an independent verifier via the `task` tool
   before declaring completion.

3. **Three-phase Plan-mode system prompt.** Replace the four-line
   description with codex-derived guidance: ground in the environment
   before asking; distinguish discoverable facts from preference/tradeoff
   unknowns; produce a decision-complete plan with the standard template
   (Summary / Key Changes / Test Plan / Assumptions). The mutating vs
   non-mutating boundary is restated so the model does not try to argue
   that running a formatter is "research."

4. **`<proposed_plan>` rendering.** Add a `Block::ProposedPlan` variant
   to the TUI document model. The parser pre-splits assistant text on
   `<proposed_plan>…</proposed_plan>` tags (treating a missing close as
   extending to end-of-input so streaming classifies the partial card
   live) and emits one such block per tag. The renderer styles it as a
   distinct card with a top border and "Proposed plan" label. The same
   content still goes to `.neenee/plans/<name>.md`; the tag is a
   presentation aid, not a replacement for the file.

`active_plan_path` is mirrored into `session.json` via a new
`SessionEvent::ActivePlanPathSet` variant, so resume restores the
Build-mode hint. `execute_turn` syncs the agent's value to the session at
every turn boundary; the `/mode` slash command syncs immediately.

## Alternatives considered

- **Keep the unconditional flip.** Rejected: the whole point of plan mode
  is that the user gets to review before the model edits. An
  unconditional flip also made the model's tendency to declare victory
  early worse, not better.
- **Auto-mode restoration (claude-code's `prePlanMode`).** claude-code
  remembers the mode the user was in before plan mode and restores it on
  exit. neenee only has `Build` and `Plan`, so there is nothing else to
  restore; the complexity is not justified. If a future mode (e.g.
  "review") is added, this decision should be revisited.
- **A dedicated `verify_plan_execution` tool.** claude-code has one.
  neenee's `task` tool already spawns clean-context read-only sub-agents,
  so the same effect is reached by telling the model in the Build-mode
  prompt to spawn a verifier itself. Avoids a new tool that mostly
  delegates to `task` anyway.
- **A TUI "Plan Implementation" popup that drives the approval.** codex
  does this. We chose the `ask_user` confirmation pattern instead because
  it is the same component the model uses for any other yes/no question,
  so there is one fewer rendering surface to maintain. The TUI still
  renders the plan distinctly via `Block::ProposedPlan`.

## Consequences

Positive:

- The model can no longer silently flip itself out of Plan mode; the user
  is the gate.
- After approval, the model knows which file to follow and can spawn a
  verifier without the user reminding it.
- Resume restores the active-plan hint, so a long implementation that
  crosses a session restart does not lose the plan context.
- The TUI transcript shows the plan as a distinct card, making it obvious
  when the model is presenting a final plan vs thinking out loud.

Negative:

- One extra round-trip per `plan_exit` (the ask_user confirmation). In
  practice this is the point, so it is a cost paid willingly.
- The Plan-mode system prompt is noticeably longer. Token cost per turn
  in Plan mode goes up by roughly the size of the new framework text.
- `<proposed_plan>` parsing adds one pre-splitting pass on every
  assistant message reparse. The fast path (no tag in the text) bails
  out before any allocation, so the cost is zero when the tag is absent.

Migration:

- Old `session.json` files load with `active_plan_path: None`. The schema
  migration is the existing `#[serde(default)]` on the new field; no
  explicit bump of `schema_version` is required for a forward-compatible
  additive field.
- Existing in-flight plan-mode sessions (rare; the project is pre-1.0)
  lose the active-plan hint on first load and behave like Build mode
  with no plan reference. Users can point the model at the plan file
  manually.

## References

- [Plan mode](../explanation/agent-design/plan-mode.md) — updated explanation of the
  end-to-end flow
- [How to plan a change](../how-to/plan-a-change.md) — step-by-step guide
- [Built-in tools](../reference/tools/index.md) — `plan_enter` and `plan_exit`
  parameter schemas
- opencode: `packages/opencode/src/tool/plan.ts`,
  `packages/opencode/src/agent/agent.ts` (plan agent + plan_exit)
- claude-code: `src/tools/ExitPlanModeTool/ExitPlanModeV2Tool.ts`
  (approval gate + plan content echo)
- codex: `codex-rs/collaboration-mode-templates/templates/plan.md`
  (three-phase workflow + decision-complete rule),
  `codex-rs/utils/stream-parser/src/proposed_plan.rs` (tag parser)
