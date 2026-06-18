# Header

The header is a floating half-block panel at the top of the chat column, shaped
like the [input box](input-box.md): 2-col `app_bg` side gutters, `▄`/`▀`
half-block transitions top and bottom, and content indented inside on
`panel_bg`.

## Appearance

```text
  ╻▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
  ┃  gpt-4o               [██████░░░░] 58% (74k/128k)
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
```

The first and last rows are half-block transitions: only the inner half of each
cell carries `panel_bg`, so the panel fades in/out of `app_bg` (see
[half-block characters](half-block-chars.md)).

- Model name in `accent` + BOLD.
- Optional goal appended: `◎ objective… [2/5]` in `text_muted`.
- Right-aligned cluster: optional MCP summary plus a context-usage bar.
- Internal indent of `HEADER_PANEL_INNER_PADDING` (2) cols inside the panel.

## Context usage bar

`[██████░░░░] 58% (74k/128k)` fills with the **used** fraction of the current
model's context window, so a nearly full bar means the window is almost
exhausted.

| Attribute | Value |
|-----------|-------|
| Window source | `ModelSolution::context_window`, looked up by provider id via `model_context_window` |
| Used estimate | `estimate_context_tokens` over the rendered messages (~4 chars/token, matching `neenee_core`) |
| Cells | 10 (`CONTEXT_USAGE_BAR_CELLS`); filled `█`, empty `░` |
| Color | `success` below 70%, `warning` 70–90%, `error_fg` above 90% |
| Hidden when | provider has no known window (`custom` / `llama` / `mock`), or panel narrower than `HEADER_CONTEXT_MIN_WIDTH` (40) |

## What it does not show

No brand name, no logo dot, no provider name, no mode badge. The model name
alone is the visual anchor.

## Height

The panel is top transition + content + bottom transition. A goal-checklist
dock adds one content row.

| Condition | Height |
|-----------|--------|
| No checklist | 3 rows (transition + content + transition) |
| With checklist | 4 rows (transition + model + checklist + transition) |
| Overlay modal open | 0 rows (hidden) |

## Source

`draw_chat` in `render/mod.rs` builds the panel inline (gutters, `▄`/`▀`
transitions, `panel_bg` content rows). Context bar from
`context_usage_spans`; window from `model_context_window` (`lib.rs`); usage
from `estimate_context_tokens` (`document.rs`). Spacing constants live in
`render/design.rs`.
