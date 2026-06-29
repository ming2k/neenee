# Thinking step

An [expandable step](expandable-step.md) for model reasoning / chain-of-thought
text. It renders flat on the app background — no band, like a
[tool step](tool-step.md) — so reasoning reads as quiet prose rather than a
panel.

## Collapsed

```text
  + Thinking · 140 chars · 1.2s
```

| Attribute | Value |
|-----------|-------|
| Background | `app_bg` (flat — no band), inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD — same disclosure marker as a tool step; the streaming state is conveyed by the summary text (duration omitted) and the steady `info` hue, never by the marker |
| Header text column | 4 from transcript edge (after the `+ ` prefix) |

The summary color is the pure weight channel from the
[state machine](step-state.md) — reasoning never carries a text accent, so
the lifecycle is conveyed by the summary text (duration appears once the
trace finishes) and the steady `info` hue. The marker is always `+`/`-`;
with the activity bar as the single breathing anchor
([ADR-0008](../../adr/0008-single-breathing-anchor.md)), nothing about the
marker needs to change between streaming and finished.

## Header format

| State | Format |
|-------|--------|
| Streaming | `Thinking · {chars} chars` (duration omitted) |
| Completed | `Thinking · {duration} · {chars} chars` |

## Expanded

```text
  - Thinking · 140 chars · 1.2s

    reasoning text in text_muted...
```

A blank row separates the header from the body; consecutive text blocks are
likewise blank-separated. Paragraph breaks inside a single block are already
preserved as empty rows by `wrap_text`.

| Attribute | Value |
|-----------|-------|
| Background | `app_bg` (flat) |
| Body indent | `TRANSCRIPT_BODY_PREFIX_COLS` (transcript column 4, left-aligned with the header text) |
| Body color | `text_muted` |
| Body style | Plain wrapped text (no code gutter) |

## Interaction

See [expandable step](expandable-step.md#behavior) for the shared toggle,
sticky-pin, and narrow-fallback behavior.

Thinking steps participate in the same keyboard focus order as tool steps.
Use `Ctrl+↑` / `Ctrl+↓` to focus a step, then `↑` / `↓` to walk focused
steps. `Enter` / `Space` opens or closes the focused thinking step.

## Source

`draw_reasoning_trace` (and `draw_reasoning_trace_header`) in
`crates/neenee-tui-view/src/render/disclosure/renderers.rs`. Header data from
`thinking_header()` in `document.rs`.
