# Step state machine

Every collapsible transcript entry — a [tool step](tool-step.md), a
[thinking step](thinking-step.md), or a sub-agent task step — is presented
through one shared state model in `crates/neenee-cli/src/tui/render/step/`.
This page documents that model: its three orthogonal axes, the two
presentation channels they reduce to, and the transitions each axis allows.
Per-tool body content lives on the [tool step](tool-step.md) page; the shared
header shape lives on the [expandable step](expandable-step.md) page.

## Why three axes

Each step kind previously computed its summary-line color from a tangle of
ad-hoc flags (`expanded`, `focused`, `hovered`, `status`…) scattered across the
data, interaction, and render layers. That conflation caused bugs like "a
collapsed step stays highlighted because it still carries keyboard focus." The
fix is to model a step as **three orthogonal axes**, each with one reason to
change, and reduce the visible presentation to pure functions of them.

| Axis | Type | Owner | Persisted? | Drives |
|------|------|-------|------------|--------|
| **Lifecycle** | kind-specific (`ToolStatus`, or `running: bool` for reasoning) | model / harness | yes | accent (hue) |
| **Disclosure** | `Disclosure` (Collapsed / Expanded) | model, user, or auto default | yes, with a `user_pinned` flag | weight + body visibility |
| **Interaction** | `Interaction` (Idle / Hovered / Focused) | pointer / keyboard hit-test | no — recomputed every frame | weight |

Lifecycle is **kind-specific** and therefore not unified here: tool steps carry
it through `ToolStatus` (5 states); reasoning traces carry it through a single
running boolean (2 states). Both resolve to an `Option<Color>` accent at the
call site and pass it in. The state module never asks "what kind of step is
this?" to pick a color.

## The two presentation channels

The summary line's color is the composition of two independent channels:

- **accent** (hue) — from Lifecycle. A non-`Ok` tool lifecycle and a denied
  step stay visibly accented even when collapsed and idle, so a running,
  failed, or denied call cannot hide. An `Ok` tool step and any reasoning
  trace yield `None`, handing control to the weight channel.
- **weight** (luminance) — from Disclosure × Interaction, via
  `summary_weight`. Decides how bright the summary reads (active vs. hover vs.
  muted), never which hue.

Keeping the channels separate is what makes behavior consistent across step
kinds and immune to the old "focus leaks into color" class of bug: keyboard
focus is a separate concern from disclosure and is conveyed by its own cue
(the `↑` / `↓` focus ring), never by stealing the open/hover luminance
channel.

## Disclosure FSM

Whether the step's body is shown. Two states, with a sticky `user_pinned`
flag on the message that gates automatic transitions:

```text
                ┌─────────────────────────────────────────────┐
                │  Auto default, re-evaluated on every         │
                │  lifecycle transition (start / finish /      │
                │  cancel). No-op once user_pinned == true.    │
                │  Writers: set_tool_step_expanded,            │
                │           set_thinking_expanded              │
                ▼                                             │
         ┌─────────────┐                                     │
         │  Collapsed  │                                     │
         │     (+)     │                                     │
         └─────────────┘                                     │
           │           ▲                                     │
   pin_*   │           │  pin_*_expanded(false)              │
 _expanded │           │  (sets user_pinned = true)          │
   (true)  │           │                                     │
           ▼           │                                     │
         ┌─────────────┐                                     │
         │  Expanded   │                                     │
         │     (-)     │                                     │
         └─────────────┘                                     │
                │                                             │
                 │  Body is painted; header may pin to the    │
                 │  top of the transcript area when scrolled  │
                 └─────────────────────────────────────────────┘
```

| State | Marker | Body | Summary weight (no accent) |
|-------|--------|------|-----------------------------|
| `Collapsed` | `+` | hidden | `theme.muted()`, or `theme.hover()` under the pointer |
| `Expanded` | `-` | visible | `theme.fg()` — expansion dominates every interaction |

### The `user_pinned` invariant

The single rule that prevents auto defaults from fighting the user:

| Writer | Used by | Effect |
|--------|---------|--------|
| `set_tool_step_expanded` / `set_thinking_expanded` | harness lifecycle transitions, step creation, scroll restore, selection-then-expand | no-op when `user_pinned == true` |
| `pin_tool_step_expanded` / `pin_thinking_expanded` | user toggle (click, `Enter`, `Space`, `Ctrl+T`) | forces `expanded` and sets `user_pinned = true` |

Once the user has manually expanded or collapsed a step, later lifecycle
transitions leave it alone. There is no explicit "unpin"; a later manual
toggle just re-pins to the new value.

### Auto defaults

Default disclosure is a pure function of `(kind, lifecycle)`, evaluated by
`step_interaction::default_tool_expanded` and `default_thinking_expanded`:

| Step kind | Lifecycle | Default disclosure | Reason |
|-----------|-----------|--------------------|--------|
| Tool | `Running` | Collapsed | no result yet; live-streaming tools still accumulate output the user can expand manually |
| Tool | `Failed` | Expanded | the error is the whole point |
| Tool | `Denied` | Expanded | the denial message must be visible without an extra click |
| Tool | `Cancelled` | Collapsed | an aborted call reads as inert |
| Tool | `Ok` | per-tool `[tui.default_expanded]` entry, or `true` under Comfortable density | `edit_file` shows its diff; `bash` / `read_file` stay collapsed |
| Thinking | streaming | Expanded | live reasoning is the value of a trace |
| Thinking | finished | Unchanged | no auto-collapse — do not yank away content the user was reading |

## Interaction FSM

Transient pointer/keyboard state for the summary line, recomputed every frame
from the layout-map hit-test. Never persisted.

```text
         pointer leaves summary
   ┌───────────┐ ◄────────────────── ┌───────────┐
   │   Idle    │                     │  Hovered  │
   └───────────┘ ──────────────────► └───────────┘
                  pointer enters summary
```

`Interaction::Focused` is reserved in the type for the keyboard focus ring but
is deliberately **not** fed into the weight channel: the
`(Collapsed, Focused)` arm reduces to `theme.muted()`, identical to
`(Collapsed, Idle)`. This is the regression guard for "collapsed focused step
stays highlighted" — focus is conveyed by its own cue (the focus ring), never
by raising the summary's luminance.

## Lifecycle accent

The accent color a renderer passes to `summary_text_color`, by source:

| Step kind | Lifecycle | Accent | Source |
|-----------|-----------|--------|--------|
| Tool | `Running` | `Some(theme.info)` — steady accent against the summary bg | `draw_tool_step` |
| Tool | `Failed` | `Some(theme.error_fg)` | `draw_tool_step`, `draw_subagent_bar` |
| Tool | `Denied` | `Some(theme.warn)` — distinct from a runtime failure | `draw_tool_step`, `draw_subagent_bar` |
| Tool | `Cancelled` | `Some(theme.text_muted)` — reads as inert, not as a fresh failure | `draw_tool_step`, `draw_subagent_bar` |
| Tool | `Ok` | `None` — hands control to the weight channel | `draw_tool_step`, `draw_subagent_bar` |
| Reasoning | streaming / finished | `None` — the lifecycle reads from the summary text (duration omitted while streaming) and the steady `info` hue; the marker is always `+`/`-`, never a streaming glyph | `draw_reasoning_trace` |

A `Some(accent)` always overrides the weight channel outright. `None` falls
through to `summary_weight`. This is what keeps a collapsed, idle, failed step
visibly red — failure must never hide behind a muted tone.

## Color resolution table

The full `summary_text_color(disclosure, interaction, accent)` truth table.
The first matching row wins:

| Disclosure | Interaction | `accent` | Summary color |
|------------|-------------|----------|---------------|
| any | any | `Some(c)` | `c` |
| Expanded | any | `None` | `theme.fg()` |
| Collapsed | Hovered | `None` | `theme.hover()` |
| Collapsed | Idle or Focused | `None` | `theme.muted()` |

## Invariants worth keeping

These are the load-bearing contracts. Breaking any of them tends to regress
one of the historical bugs the state machine was introduced to fix:

- **One reason to change per axis.** Lifecycle changes do not write
  `user_pinned`; user toggles do not mutate Lifecycle. The only thing that
  crosses the seam is the auto-default re-evaluation, and it goes through the
  pinned-gated setter.
- **Accent overrides weight, never the reverse.** A failed step stays red
  whether collapsed, idle, or under the pointer.
- **Focus never brightens.** `Interaction::Focused` exists in the type to keep
  the match exhaustive, but it collapses to the same weight as `Idle`.
- **Expansion dominates interaction.** An open step is always the primary
  foreground; the pointer state cannot dim it.
- **Reasoning never carries a text accent.** Its lifecycle is marker-only, so
  the weight ladder stays meaningful on reasoning summaries.

## Source

| File | Responsibility |
|------|----------------|
| `render/step/state.rs` | `Disclosure`, `Interaction`, `summary_weight`, `summary_text_color`. Pure functions, unit-tested in isolation from rendering |
| `render/step/mod.rs` | The three-axes architectural overview and the public re-exports |
| `render/step/renderers.rs` | Concrete step renderers that feed the axes in: `draw_tool_step`, `draw_reasoning_trace`, `draw_subagent_bar`, `draw_subagent_inline_step` |
| `render/tools/mod.rs` | `ToolStatus` (5 states), `ToolStatus::color` |
| `step_interaction.rs` | `default_tool_expanded`, `default_thinking_expanded`, summary-at-pointer classification (`summary_at`, `hovered_summary`) |
| `document.rs` | `set_*_expanded` (auto, no-op if pinned) and `pin_*_expanded` (user, sets `user_pinned`); the `user_pinned` field on `MessageKind::ToolStep` / `MessageKind::Thinking` |
| `app.rs` | `toggle_step_pinned` — wires the user toggle to the pin setter and the sticky-scroll keep-anchored behavior |
