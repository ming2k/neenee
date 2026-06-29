# Envoy view

When the user zooms into an `envoy` tool call, the TUI swaps the root
conversation for that envoy's child messages, hides the footer, and pins a
one-row navigation bar at the bottom of the transcript area. This page
documents that view's layout and key bindings. The isolation model and
event streaming that produce the children live on the
[Envoys](../../explanation/agent-design/envoys.md) explanation page;
the inline (non-zoomed) rendering of the task step lives on the
[Tool step](tool-step.md#envoy-children) page.

## Two render modes

An `envoy` tool step has two render modes, switched by the
[focus stack](#focus-stack):

| Mode | When | What renders |
|------|------|--------------|
| **Inline** (default) | Focus stack does not include this step's call id | `draw_envoy_inline_step` — a one-line summary plus an optional live status line, flat on `app_bg` |
| **Zoomed** | Focus stack's top is this step's call id | The task's `envoy_children()` become the message stream; the footer is hidden and a navigation bar is drawn at the bottom of the transcript area |

The inline step never expands inline. `Enter` on a focused inline task step
navigates into the zoomed view rather than toggling a body — the step is
registered as a tool-step summary (sentinel `block_idx = usize::MAX`) purely
so the existing click / `Enter` machinery recognizes it.

## Inline step

```text
  ↳ Explore the codebase to find the login bug
  ↳ Running: grep -n "session" src/auth
```

| Attribute | Value |
|-----------|-------|
| Background | `theme.surface()` (`app_bg`), inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | None — the step navigates; disclosure is conveyed by Enter/click, not `+`/`-` |
| Summary color | `summary_text_color(accent, Collapsed, Hovered?)` via the shared [step state machine](step-state.md); `Running` reads as a steady `info` accent (no per-step breathing — see [ADR-0008](../../adr/0008-single-breathing-anchor.md)) |
| Status line | Wrapped, `theme.muted()`, indented 2 cols; the whole line is part of the same clickable summary so clicking anywhere enters the zoom |
| Lifecycle accent | Same wiring as a tool step: `Ok → None`, `Failed → Some(theme.error_fg)`, `Denied → Some(theme.warn)`, `Cancelled → Some(theme.text_muted)`, `Running → Some(theme.info)` |

The live status line comes from `TranscriptMessage::envoy_status_line`
(e.g. `↳ Running: grep foo`, `↳ Completed · 3 calls`).

## Zoomed view

```text
┌──────────────────────────────────────────────────────────┐
│                                                          │
│   ... the focused task's child messages, rendered        │
│   exactly like the root conversation (user messages,     │
│   assistant text, tool steps, thinking steps, ...)       │
│                                                          │
├──────────────────────────────────────────────────────────┤
│ Task  explore the codebase  (1 of 3)   Esc back  [ prev  ] next │  ← envoy bar
└──────────────────────────────────────────────────────────┘
```

The message stream above is rendered by the same `draw_transcript` pass as
the root conversation, just against `focused_messages()` instead of
`self.messages`. The footer (activity bar, input box, hint bar)
collapses to 0 height — the zoomed view is read-only, and the navigation
bar is its only chrome. See [Frame layout → Envoy zoom view](layout.md#envoy-zoom-view)
for the rect math.

### Envoy bar

Drawn by `draw_envoy_bar` at the bottom of the transcript chunk, across
the full transcript width inside the `app_bg` gutters. The layout is
left / spacer / right:

| Region | Contents | Style |
|--------|----------|-------|
| Left label | ` Task` | `fg` bold on `theme.body()` |
| Description | the focused task's label | `theme.brand()` |
| Sibling count | ` (N of M) ` when `M > 1`, else a single space | `theme.muted()` |
| Spacer | pad to fill the row | `theme.body()` |
| Right hint | `Esc back   [ prev   ] next ` | `theme.muted()` |

The bar uses `theme.body()` (not `theme.panel()`) so it reads as a thin
navigation strip rather than as another modal panel.

## Focus stack

The focus stack is a `Vec<String>` of `envoy` call ids stored on `App`. It
is the source of truth for "which conversation are we looking at":

| State | `focused_messages()` returns | `in_envoy_view()` |
|-------|------------------------------|----------------------|
| Empty | `&self.messages` (the root conversation) | `false` |
| Non-empty | The `envoy_children()` of the root-level `envoy` step whose call id equals the stack's top | `true` |

Transitions are entirely caller-driven — the renderer never pushes or pops.
The stack supports nesting: zooming into an `envoy` that itself spawned a
`envoy` pushes a second call id, and the focused slice is the innermost
envoy's children.

| Action | Effect on focus stack |
|--------|-----------------------|
| `Enter` / click on an inline `envoy` summary | Push that step's call id; `reset_view_state` clears scroll, selection, sticky pinning |
| `Esc` from a zoomed view | Pop the top; if the stack is now empty, restore the root view |
| `[` (left bracket) | Pop the top and re-push the previous sibling's call id — cycle to the previous sibling task at this depth |
| `]` (right bracket) | Pop the top and re-push the next sibling's call id — cycle to the next sibling |

When cycling siblings, the bar's `(N of M)` indicator reflects the new
sibling's 1-based position among the parent's child tasks. The previous
scroll position is not restored — each sibling enters with
`reset_view_state`, since the streams are unrelated.

## Re-entering an existing envoy

If the user re-opens an envoy that already has children (e.g. an envoy that
finished earlier in the session), the existing children are shown
immediately — they are persisted on the message via `envoy_children()`.
Live updates still go through `push_subtask_event`; the resume path uses
`attach_envoy_children` to rebuild the nested view from persisted
storage. See `document.rs` for both entry points.

## Source

| File | Responsibility |
|------|----------------|
| `render/disclosure/renderers.rs` | `draw_envoy_inline_step`, `draw_envoy_bar` |
| `render/mod.rs` | `EnvoyBarInfo`, wiring the bar into `draw_transcript` when `view.envoy_bar` is `Some` |
| `app.rs` | `focus_stack`, `in_envoy_view`, `focused_messages`, `reset_view_state` |
| `document.rs` | `is_envoy_task`, `tool_step_call_id`, `envoy_children`, `envoy_children_mut`, `attach_envoy_children`, `envoy_status_line` |
| `input.rs` | `in_envoy_view` flag on `InputContext`, used so `Enter` on an inline envoy step navigates instead of submitting the composer |
