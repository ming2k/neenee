# 0045. Extract `neenee-tui-view` (widgets + document model) from the app shell

- **Status:** Accepted
- **Date:** 2026-07-10
- **Builds on:** ADR-0038 (in-house grid engine `neenee-tui`), ADR-0035
  (the application-layer split that named the `neenee-code` shell)

## Context

ADR-0038 replaced ratatui with an in-house engine, `neenee-tui`, and folded
all of the terminal UI — engine, widgets, document model, *and* application
shell — into a single `tui/` module tree inside the `neenee-code` binary
crate. That was the right call at the time: the priority was shipping the
engine, not slicing it.

That co-location left no boundary between *rendering* and *application
wiring*, which produced three concrete pain points as the codebase grew:

1. **Untestable widgets.** ~5,000 lines of rendering logic
   (`render/`, `document.rs`, `layout.rs`, `selection.rs`, `fuzzy.rs`,
   `providers.rs`) could only be exercised through the shell's event loop,
   because they were private modules of the binary crate. The snapshot
   tests that did exist had to spin up the whole `App`.

2. **No enforced one-way dependency.** Nothing stopped a renderer from
   reaching into `App` or the input/event-loop modules. The intended
   invariant — "widgets are a pure function of borrowed data, with no
   back-channel into application state" — was a convention the compiler
   could not check. Over time a couple of read-only crossings
   (`config::tool_default_expanded` → `render::tools::presenter_for`,
   `transcript.rs` coupling the document builder to `TuiConfig`) had crept
   in precisely because nothing made the boundary concrete.

3. **A fat shell.** `neenee-code::tui` carried ~18,000 lines spanning four
   unrelated concerns — rendering, the semantic document model, input
   mapping, and session/event-loop wiring — in one crate, which is exactly
   the "unsplit `neenee-tui`" pressure ADR-0038 said it would revisit.

## Decision

Carve a new crate, **`neenee-tui-view`**, out of `neenee-code::tui`. It owns
everything that is a pure function of borrowed data — the widget tree
(`render/`), the semantic document model (`document`), the layout and
hit-testing map (`layout`), text/cell selection (`selection`), the fuzzy
matcher (`fuzzy`), provider/model picker ranking + display helpers
(`providers`), the shared modal discriminants (`modal`), and the
completion-menu data *types* (`completion`). Its dependencies are exactly:

```text
neenee-tui-view  →  neenee-tui (draw into the grid)
                 →  neenee-core (the domain types it renders)
                 →  neenee-providers (the model catalog the picker ranks)
```

It **never** depends on `neenee-code`; the compiler now enforces the one-way
boundary that was previously only convention.

### The seam: `render::TranscriptView<'a>`

The shell and the view communicate through one borrowed struct,
`render::TranscriptView<'a>`, that the event loop fills in each frame from
its snapshot. It carries only borrowed data — `&[TranscriptMessage]`,
`&SelectionState`, `&Theme`, scroll/activity/pursuit/todo snapshots — and
crucially **no reference to `App`**. `render::draw_transcript(frame, &mut
LayoutMap, view)` is the single entry point for the transcript; the
per-modal overlays (`draw_models_modal`, `draw_permission_sheet`, …) take
their own small borrowed view structs (`HintBarView`, `ActivityModalView`,
`CustomEditorView`, …) the same way. The call is always shell → view; the
view writes hit-regions back into a borrowed `LayoutMap`/`ModalHitMap` that
the shell owns and reads for click routing.

### Historical paths preserved

To keep the migration a pure mechanical move, the view's modules are
re-exported at their historical `crate::tui::*` paths inside the shell:

```rust,ignore
pub(crate) use neenee_tui_view::{document, fuzzy, layout, providers, render, selection};
```

So `crate::tui::render::draw_transcript` still resolves — it now points
across the crate boundary instead of into a sibling module.

### Two deliberate shell-side residents

Not everything rendering-adjacent moved. Two pieces stay in the shell by
design, because they couple to types the view layer must not depend on:

- **`transcript.rs`** — the `core::Message → document::TranscriptMessage`
  builder. It reads `TuiConfig` (which lives in `neenee-store`) and applies
  the lifecycle-aware disclosure policy from `step_interaction`, so it
  cannot live below the shell without dragging the store into the view.
- **`config.rs::tool_default_expanded`** and `step_interaction.rs` — the
  *presentation policy* that combines a raw `[tui.default_expanded]` entry
  with each tool's built-in presenter default. This reads the presenter
  registry (`render::tools::presenter_for`), which is a downward
  (view-ward) call — allowed — and is the single place the shell consults
  a view-layer function for a decision rather than a draw.

These are the *only* crossings; both are shell → view, never view → shell.

## Alternatives considered

- **Keep everything in `neenee-code::tui`.** Rejected: leaves the three
  pain points (untestable widgets, unenforced boundary, fat shell) intact.
  This ADR exists precisely because the co-location stopped paying off.

- **Fold the widgets into the engine crate `neenee-tui`.** Rejected:
  ADR-0038 holds that the engine must "know nothing about neenee." The
  widget layer renders `neenee-core` domain types (`TranscriptMessage`,
  `PermissionRequest`, …), so merging them would re-pollute the engine
  with neenee dependencies and undo the engine's generality.

- **Move the document-model builder (`transcript.rs`) and presentation
  policy (`config.rs`) into the view too.** Rejected: both couple to
  `neenee-store` (`TuiConfig`) and the shell's own `step_interaction`. The
  view layer must not depend on the store; forcing it would invert the
  layering. Leaving them in the shell keeps the view's dependency list to
  `core`/`providers` only. Recorded here so a future cleanup does not
  "finish" the migration by moving them.

- **An interface trait abstracting the shell from the view.** Rejected as
  over-engineering: there is exactly one shell and one view, the data flow
  is strictly shell → view, and `TranscriptView<'a>` (plain borrowed
  fields, no `&App`) already prevents the back-channel a trait would guard
  against. A trait would add indirection for no second implementation.

## Consequences

- **Positive — isolated, fast widget tests.** The view crate now carries
  181 unit tests including its full render snapshot suite, runnable with
  `cargo test -p neenee-tui-view` with no `App` or terminal involved. New
  widgets gain the same quick feedback loop.

- **Positive — compiler-enforced one-way seam.** A renderer can no longer
  accidentally reach into `App` or the event loop; the build fails if it
  tries. The two intentional shell-side crossings are documented above.

- **Positive — smaller, focused shell.** `neenee-code::tui` drops to the
  concerns it actually owns (state, event loop, input, terminal lifecycle,
  session wiring), ~18k → ~13k lines, with a clean re-export shim keeping
  call sites unchanged.

- **Neutral — two shell-side residents.** `transcript.rs` (document-model
  construction) and `config.rs`/`step_interaction.rs` (presentation policy)
  stay in the shell and call downward into the view. This is a documented
  seam, not a leak: both calls are shell → view.

- **Neutral — historical paths via re-export.** Shell code reads
  `crate::tui::render::*` exactly as before; the indirection is one
  `pub(crate) use`. Call sites did not change, so the diff stayed a pure
  extraction.

### Migration steps (completed)

1. Create `crates/neenee-tui-view` with `neenee-tui`, `neenee-core`, and
   `neenee-providers` as its only dependencies.
2. Move `render/`, `document.rs`, `layout.rs`, `selection.rs`, `fuzzy.rs`,
   `providers.rs`, `modal.rs`, and `completion.rs` (types only) into it.
3. Add `pub(crate) use neenee_tui_view::{…}` re-exports in the shell so
   `crate::tui::*` paths resolve unchanged.
4. Add `neenee-tui-view` to the workspace `members` and to
   `neenee-code`'s dependencies.
5. Update the reference docs (`docs/reference/tui/architecture.md` and the
   `tui/` reference pages) to the three-layer model; add this ADR.

## References

- [ADR-0038](0038-in-house-grid-diff-rendering-engine.md) — the engine
  layer (`neenee-tui`) this crate renders into.
- [ADR-0035](0035-application-layer-split.md) — the application-layer
  split that named the `neenee-code` shell this crate was carved from.
- [TUI architecture](../reference/tui/architecture.md) — the layer/module
  reference this decision is reflected in.
- [Terminal UI](../explanation/tui.md) — the conceptual overview of the
  three-layer split and the `TranscriptView` seam.
