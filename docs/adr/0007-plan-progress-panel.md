# 0007. Plan progress sticky panel

- **Status:** Superseded by ADR-0020
- **Date:** 2026-06-21

## Context

ADR-0006 shipped the plan-mode v2 foundation: an approval gate on
`plan_exit`, an `active_plan_path` recorded into the session, and the
`<proposed_plan>` block rendered inline in the transcript. One piece was
still invisible to the user during implementation: **how much of the plan
is done**.

The Build-mode system prompt already told the model which plan file to
follow, but the user only saw that hint as raw text in the transcript (and
only on the turn right after approval). Once the model started editing
files there was no UI surface for "we are 2 of 4 sections in" — the user
had to scroll back to find the `<proposed_plan>` card or open the plan
file in another tool.

Three reference projects were surveyed:

- **opencode / claude-code**: no persistent progress indicator. The plan
  lives in the transcript; the model just keeps going.
- **codex**: emits a `<proposed_plan>` tag the TUI renders specially, plus
  an optional Plan Implementation popup. No per-section progress.
- **Cursor / Cline (IDE)**: persistent sidebar panels with per-step
  progress, but they are GUI apps with room to spare. A TUI is much
  tighter.

The question was whether a TUI sticky panel above the input box would add
enough value to justify the screen real estate, and how to avoid the
"stale progress bar" trap where the model forgets to update it and the
user is misled.

## Decision

Add a 3-row sticky panel above the input box that renders whenever an
active plan exists (`Agent::plan_progress().is_some()`). The panel shows
the plan path, the section completion ratio, and one row of per-section
status glyphs. Sections that do not fit are elided with `…` so the input
box height never jumps.

```
╭── Plan: rewrite-auth.md · 1/4 done ───────────────╮
│ ✓ Summary  ● Key Changes  ○ Test Plan  ○ Assump… │
╰───────────────────────────────────────────────────╯
```

Progress is **model-driven, not inferred**. The system prompt instructs
the model to call `update_plan_progress(section, status)` whenever it
starts or finishes a section. A section it forgets to mark stays
`Pending` — which is honest (the work has not been verified) rather than
a stale auto-progress that misleads.

### Data model (in `neenee-core/src/plan.rs`)

```rust
pub enum PlanSectionStatus { Pending, InProgress, Done, Skipped }

pub struct PlanSection { pub name: String, pub status: PlanSectionStatus }

pub struct PlanProgress {
    pub path: PathBuf,
    pub sections: Vec<PlanSection>,
}
```

`PlanProgress::from_markdown` parses the approved plan into sections (one
per `##` heading; falls back to a synthetic "Plan" section when there are
no level-2 headings). `PlanProgress::update` matches a case-insensitive
substring so the model does not have to echo the exact heading.

### State flow

```
Agent ──(Arc<Mutex<Option<PlanProgress>>>)── PlanToolContext
   │
   ├── plan_exit (approved) ── parses plan markdown, seeds progress
   ├── plan_enter            ── clears progress (old plan invalid)
   ├── update_plan_progress  ── mutates one section's status
   └── emit_plan_progress_change ── AgentEvent::PlanProgressUpdated
                                   └─ AgentResponse::PlanProgressUpdated
                                      └─ UiRuntime.plan_progress (Arc)
                                         └─ App.plan_progress (per-frame copy)
                                            └─ TranscriptView.plan_progress
                                               └─ draw_plan_panel
```

### Persistence

`SessionData` gains a `plan_progress: Option<PlanProgress>` field and a
new `SessionEvent::PlanProgressSet` variant. `execute_turn` syncs the
agent's value to the session at every turn boundary (cheap comparison,
write only when changed). On resume the value is restored into the agent
so the panel re-appears in the same state.

### Rendering

The panel is a 3-row `╭ ─ ╮` / `│` / `╰ ─ ╯` card using the brand color
(`theme.brand()`) for borders and the raised surface (`theme.raised()`)
for background. Status glyphs are colored per-state — `theme.ok()` for
`Done`, `theme.warn()` for `InProgress`, `theme.muted()` for `Pending` /
`Skipped` — so progress pops at a glance. The panel is hidden in
sub-agent view (the plan belongs to the parent context) and while chrome
is hidden (overlay modal open).

## Alternatives considered

- **A. Presence-only indicator (just plan name + mtime).** Rejected as
  too thin — the user can already see the plan path in the transcript.
  Adding 3 rows of screen real estate needs to buy more than that.
- **B. Bullet-level checklist (codex `update_plan`).** Rejected as too
  brittle at this layer — the model would have to update many rows per
  turn, and one missed update leaves the bar materially wrong. Section
  granularity (4 ± 2 entries) is the right unit: coarse enough that the
  model reliably updates, fine enough to be useful.
- **C. Auto-infer progress from edits.** Rejected — a stale auto-bar is
  worse than no bar. The model is the source of truth; if it forgets to
  update, `Pending` is the honest answer.
- **D. Side panel instead of above-input strip.** Rejected for a TUI —
  side panels eat horizontal space, which is the more precious dimension
  in 80-column terminals. Above-input mirrors how opencode/claude-code
  surface ephemeral status (the existing status bar already lives there).
- **E. Clique-coded "stale detector".** Considered (grey out the panel
  if not updated for N turns) but deferred — at section granularity the
  problem is much smaller than at bullet granularity, and a stale section
  already shows as `Pending` which is informative on its own.

## Consequences

Positive:

- The user always knows which plan is being implemented and roughly how
  far along it is, without scrolling or opening another tool.
- Section status is the model's honest report, not an inference — so a
  stuck section reads as `Pending` rather than a misleading green check.
- Resume restores both the plan path and the section list, so a long
  implementation crossing a restart picks up where it left off visually.
- The panel doubles as a clickable affordance for future work (jump to
  plan file, trigger verifier, manually mark a section).

Negative:

- 3 rows of vertical real estate whenever a plan is active. Hidden when
  no plan is set, so the cost is paid only during implementation.
- One extra event in the agent → TUI relay per `update_plan_progress`
  call. Cheap, but it does mean each progress update triggers a re-draw.
- The model can game the panel by marking sections `Done` prematurely.
  The Build-mode prompt instructs it to spawn an independent verifier
  before declaring complete, which is the mitigation. A future
  "adversarial verifier" mode could independently re-mark sections, but
  that is out of scope here.

Migration:

- Old `session.json` files load with `plan_progress: None`. The schema
  migration is the existing `#[serde(default)]` on the new field; no
  explicit bump of `schema_version` is required.
- The panel only appears once the user approves a `plan_exit` after
  upgrading. Existing in-flight implementations continue without it.

## References

- [ADR-0033](0033-remove-plan-and-verify-workflow.md) — Plan mode was later
  replaced (ADR-0027) and removed.
- [ADR-0006](0006-plan-mode-v2.md) — approval gate + active plan path +
  `<proposed_plan>` rendering, of which this is a direct follow-up
- codex `update_plan` tool (`codex-rs/core/src/tools/handlers/plan.rs`)
  for the section-status enum shape
- claude-code's `ExitPlanModeV2Tool` for the pattern of "tool mutation
  emits a live event the TUI mirrors"
