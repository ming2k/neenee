# TUI architecture

The neenee terminal UI is split into **three layers**, each its own
compilation unit, with dependencies pointing strictly downward. The split
exists so the rendering engine, the widgets, and the application wiring can be
reasoned about (and tested) in isolation, and so the widget layer can never
secretly reach into application state.

```text
┌──────────────────────────────────────────────────────────────────────┐
│  neenee-tui  ·  ENGINE                                  (ADR-0038)     │
│  Retained cell grid · write-marks-dirty tracking · back/front diff ·   │
│  crossterm backend · Frame / Rect / Layout / Span primitives.         │
│  Knows nothing about neenee — pure terminal drawing.                  │
└──────────────────────────────────────────────────────────────────────┘
                          ▲  widgets render *into* the grid
                          │  (Frame::render_widget)
┌──────────────────────────────────────────────────────────────────────┐
│  neenee-tui-view  ·  VIEW (widgets + document model)                  │
│  render/ widget tree · document model · layout/hit-testing ·          │
│  selection · fuzzy · provider ranking · shared modal discriminants.   │
│  Renders neenee_core domain types → depends on neenee-core.           │
│  NEVER depends on the app shell.                                      │
└──────────────────────────────────────────────────────────────────────┘
                          ▲  the shell fills in a borrowed
                          │  TranscriptView<'a> each frame
┌──────────────────────────────────────────────────────────────────────┐
│  neenee-code::tui  ·  APP SHELL                                       │
│  App state · event loop · input→action mapping · terminal lifecycle · │
│  completion logic · clipboard · session wiring.                       │
│  Owns the data; drives the view layer; depends on neenee-tui-view.    │
└──────────────────────────────────────────────────────────────────────┘
```

## The three layers

### Engine — `crates/neenee-tui`

The in-house grid engine (ADR-0038). A retained 2-D cell grid with
write-marks-dirty tracking, a back/front buffer diff, and a crossterm backend.
It exposes `Frame`, `Rect`, `Layout`, `Span`, `Style`, `Grid`, `TestTerminal`,
and friends. It has **no neenee dependencies** — it is a general terminal
drawing engine that the view layer paints into.

### View — `crates/neenee-tui-view`

The widget layer and the semantic document model. Everything here is a pure
function of borrowed data: it reads `neenee_core` domain types and a `Theme`
and writes cells into the engine's grid. It depends on `neenee-tui` (to draw),
`neenee-core` (the domain types it renders), and `neenee-providers` (the model
catalog the picker ranks). It **does not** depend on `neenee-code` — the
compiler enforces the one-way boundary.

| Module | Responsibility |
|--------|----------------|
| `render/` | The widget tree (transcript, steps, tools, overlays, chrome, composer). Entry point `render/mod.rs`. |
| `document` | Semantic document model: `TranscriptMessage`, `Block`, `MessageKind`, markdown parsing. |
| `layout` | `LayoutMap`, `BlockRegion`, `SemanticCursor`, hit-testing. |
| `selection` | `SelectionState`, text/cell selection, character-boundary snapping. |
| `fuzzy` | Fuzzy matcher used by the history / provider overlays. |
| `providers` | Provider/model picker ranking + display helpers (`model_display_name`, …). |
| `modal` | Shared discriminants: `Modal`, `Recess`, `ActivityTab` (see below). |
| `completion` | Completion-menu data types (`Completion`, `CompletionKind`) — the *matching logic* stays in the shell. |

### App shell — `crates/neenee-code/src/tui`

The application: `App` state, the event loop, input→action mapping, terminal
lifecycle, completion logic, clipboard, and session wiring. It owns all the
mutable state and drives the view layer once per frame. It depends on
`neenee-tui-view` and re-exports the view modules at their historical
`crate::tui::*` paths so shell code reads unchanged.

| Module | Responsibility |
|--------|----------------|
| `app.rs` | `App` state, `CaretOwner`, scroll/zoom snapshots. |
| `event_loop.rs` | App loop: state sync, draw orchestration, action handling. |
| `input/` | Event→`InputAction` keyboard/mouse dispatch. |
| `terminal.rs` | Raw-mode / alt-screen setup-teardown, render-loop wiring. |
| `completion.rs` | Slash-command / `@path` completion **logic** (`impl App`); the data types live in the view layer. |
| `step_interaction.rs` | Transcript-step focus, toggle, keyboard interaction. |
| `clipboard.rs` / `clipboard_ops.rs` | OSC52 + system clipboard, async copy. |
| `question_model.rs` | Question-modal state machine. |

## The seam — `TranscriptView<'a>`

The shell and the view layer communicate through one borrowed struct,
`render::TranscriptView<'a>`, that the event loop fills in each frame. It
carries **only borrowed data** — `&[TranscriptMessage]`, `&SelectionState`,
`&Theme`, scroll/activity/pursuit/todo snapshots — and crucially **no
reference to `App`**. This is what keeps the view layer a pure rendering
function: there is no back-channel into application state, so a widget can
only draw what the shell chose to hand it.

`draw_transcript(frame, &mut LayoutMap, view)` is the single entry point
for the transcript; the per-modal overlays (`draw_models_modal`,
`draw_permission_sheet`, …) take their own small borrowed view structs
(`HintBarView`, `ActivityModalView`, `CustomEditorView`, …) the same way.
The shell calls the view and never the reverse.

## Shared discriminants — why `modal` lives in the view layer

`Modal`, `Recess`, and `ActivityTab` are fieldless enums that *name* things
without owning state:

- `Modal` — which overlay is open. The view layer needs it for modal geometry
  (`modal_area`) and per-modal rendering; the shell needs it as state.
- `Recess` — how the live surface recedes behind a modal (float / dim /
  takeover). The view layer's recess pass and the shell's footer-collapse
  decision both key off it.
- `ActivityTab` — which section the Activity modal shows.

Because both layers share them and dependencies point downward, they live in
the lower layer (`neenee-tui-view::modal`) and the shell re-exports them. Same
reasoning for `completion::{Completion, CompletionKind}`: the render code draws
them, so the *types* live in the view layer while the *matching logic* stays in
the shell as an `impl App`.

## Component reuse inside the view layer

Within `render/`, components stack into reuse tiers — lower tiers know nothing
about higher ones:

```text
  leaves    tools/*  ·  overlays/{help,session,provider,…}
              │ build on
  mid-tier  disclosure/  ·  overlays/common  ·  composer  ·  chrome
              │ build on
  base      primitives  ·  text_layout  ·  markdown_table
              │ tokens
  tokens    theme (colors)  ·  design (spacing/gutters/row counts)
```

- **`primitives`** — `viewport_rect`, `centered_rect`, `panel_block`,
  `recess_backdrop`, `modal_area`, color helpers. The shared rect/panel/color
  vocabulary everything else is built from.
- **`text_layout`** — `wrap_text`, `WrappedLine`, `line_spans`, the
  gutter/wrapping core reused by message bodies, code blocks, and tools.
- **`theme` / `design`** — the only places colors and fixed measurements are
  defined; every component reads tokens from here instead of hard-coding.
- **`disclosure/`** — the collapsible-step state machine (`Disclosure`,
  `Interaction`) and shared header rendering, reused by every `tools/*` renderer.
- **`overlays/common`** — modal frame/header/scroll helpers reused by every
  modal in `overlays/`.
- **`render/layout/`** — transcript arrangement strategies (`compact`,
  `turn_band`) selected by `[tui] transcript_layout`.

The leaves (`tools/*`, the per-modal overlays) are intentionally thin: they
compose the mid-tier and base helpers rather than re-implementing wrapping,
panels, or color logic.

## See also

- [ADR-0038](../../adr/0038-in-house-grid-diff-rendering-engine.md) — the engine.
- [index.md](index.md) — component reference and the full source-file map.
- [layout.md](layout.md) — frame measurements, footer stack, modal modes.
- [step-state.md](step-state.md) — the disclosure/interaction state machine.
