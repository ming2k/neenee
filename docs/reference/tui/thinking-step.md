# Thinking step

An [expandable step](expandable-step.md) for model reasoning / chain-of-thought
text. It renders flat on the app background — no band, like a
[tool step](tool-step.md) — so reasoning reads as quiet prose rather than a
panel.

## Collapsed

```text
  + Thinking · 1.2s · 140 chars
```

| Attribute | Value |
|-----------|-------|
| Background | `app_bg` (flat — no band), inset 2 cols (`TRANSCRIPT_H_INSET`) |
| Marker | `+` (collapsed) / `-` (expanded), BOLD |
| Header text | `text_muted` normally; brightens to `text` on hover/focus |
| Header text column | 4 from transcript edge (after the `+ ` prefix) |

## Header format

| State | Format |
|-------|--------|
| Streaming | `Thinking · {chars} chars` (duration omitted) |
| Completed | `Thinking · {duration} · {chars} chars` |

## Expanded

```text
  - Thinking · 1.2s · 140 chars

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
`Enter` / `Space` opens or closes the focused thinking step.

## Source

`draw_reasoning_trace` (and `draw_reasoning_trace_header`) in
`crates/neenee-tui/src/render/turn_artifacts.rs`. Header data from
`thinking_header()` in `document.rs`.
