# 0001. Tool-step rendering redesign: log entries over expandable cards

- **Status:** Implemented
- **Date:** 2026-06-19

## Context

The TUI renders each tool step as an "expandable card" assembled in
`crates/neenee-tui/src/render/turn_artifacts.rs` (~2200 lines). The design has
accumulated several structural problems:

1. **Expand/collapse is the wrong axis.** State is a per-message `expanded: bool`
   (`document.rs`), toggled by *message index*. Indices shift on compaction or
   history restore, so cached toggle targets go stale. Most tools' collapsed
   state is a one-line header with no preview, so the effective choice is
   "nothing vs. a full-screen dump."
2. **Tool results are `Result<String, String>`.** The TUI recovers structure by
   string-sniffing: `output.starts_with("Error")` (duplicated four times) for
   failure, `"Exit "` / `"STDERR:"` / `"[Output truncated"` prefix matching for
   bash, and the diff is derived from `old_string`/`new_string` by hand. This
   couples the renderer to magic labels emitted by `neenee-core`.
3. **`ResultKind` is a tool-level constant** with five variants, so `read_file`,
   `webfetch`, `websearch`, the goal tools, and every MCP tool collapse to a
   flat line-numbered code block. The renderer cannot adapt to *content*.
4. **No streaming.** While a tool runs, only a braille spinner updates; the full
   output arrives in one shot via `AgentResponse::ToolResult`.
5. **Background hierarchy does not read.** A single card stacks four
   near-identical charcoal backgrounds (`app_bg`, `element_bg`, `menu_bg`,
   `code_bg`) within ~8 luminance steps, plus two overlapping palette
   generations in `Theme`.
6. **The diff is naive** (prefix/suffix only, no intra-line highlight, no hunk
   headers, not selectable), so small edits look as noisy as full rewrites.

## Decision

Replace the expandable-card model with a **log-entry** model inspired by codex
and opencode, founded on six pillars:

1. **Log entry, not widget.** A tool step is one header line plus an optional,
   *bounded* body (head+tail truncation). Density (`compact` | `comfortable`) is
   a global, persisted mode — not per-card state. Full detail lives in a
   per-step overlay opened via `Ctrl+T` (repurposed from bulk toggle).
2. **Structured output.** Evolve `Tool::call` to return a typed `ToolOutput`
   enum (`Prose`, `Code`, `Shell`, `Patch`, `Listing`, `Matches`, `Links`,
   `Checklist`, `Kv`, `Error`). A `From<String>` back-compat path lets tools
   migrate incrementally. The TUI renders from data, never by string-sniffing.
3. **Content-adaptive bodies.** Body renderers are chosen by content kind (and
   may adapt to the payload, e.g. HTML vs. JSON for `webfetch`), not solely by
   tool name.
4. **One surface, luminance hierarchy.** Drop multi-band backgrounds; use a
   single `surface` plus dim/bold and a single accent rail for true panels.
   Introduce a semantic color layer so renderers reference intent (`ok`, `warn`,
   `err`, `muted`, `accent`) rather than raw `Color` fields, and merge the
   duplicated/legacy `Theme` tokens.
5. **Stream everything.** Add a streaming channel (`ToolStream::Stdout` /
   `Stderr` / `Replace`) so `bash`, `grep`, and `listing` grow live; the spinner
   is replaced by a "breathing" header-color sweep while running.
6. **Header grammar.** `+  verb  summary  meta` (collapsed) / `-  verb  summary  meta`
   (expanded), e.g. `+  Read  crates/main.rs  · 0ms`. A `+` / `-` marker column
   indicates expand state. Status is conveyed by header color only: a breathing
   accent while running, `error_fg` on failure, `text_muted` when cancelled,
   and neutral on success. No per-tool icons or status glyphs.

Supporting refactor: introduce a `RenderCtx` carrying the frame, area, theme,
width, density, and the `skip_rows`/`y` scroll accumulators, plus a single
`emit_row`/`emit_rows` primitive. This removes the seven duplicated skip/clip
loops and drops primitive signatures from 13–17 positional arguments to one.

### Per-tool catalog (summary)

`bash` → `$ Run` + framed stdout/stderr + exit/truncation markers (streamed).
`read_file` → `→ Read` + line-numbered code. `write_file`/`edit_file` →
`← Write/Edit (+N −M)` + real unified/split diff with hunk headers and
intra-line word highlighting. `grep` → `✱ Grep (N)` + matches grouped by file.
`glob`/`list_dir` → `✱ (N)` + colored listing. `webfetch` → `% Fetch` +
markdown/raw by content type. `websearch` → `◈ Search (N)` + link list.
`todo`/`goal_checklist` → `⚙ Todos` + `[✓•☐✕]` list. `task` → single inline
card with a live `↳ <current child tool>` subline; internals reached by entering
the child session. MCP/unknown → `· server/tool`, body hidden behind an opt-in
`generic_output_visibility` flag.

## Alternatives considered

- **Keep the expandable card, fix incrementally.** Rejected: the per-card state
  model, the `String` result type, and the `ResultKind` constant are the root
  causes; patching them piecemeal preserves the structural debt.
- **Per-card expand with stable ids (not indices).** Rejected: retains the
  binary collapsed/expanded axis, which is itself the problem — coding agents
  produce many tool calls and per-call expand state is noise. A global density
  mode plus an overlay scales better.
- **Full codex-style "no borders anywhere."** Partially adopted: borders remain
  for genuine panels (plan, proposed plan, session header) to preserve neenee's
  identity, but are removed from tool steps.

## Consequences

- Positive: eliminates string-sniffing and index-based toggle state; enables
  streaming; makes the transcript calmer and more scannable; shrinks
  `turn_artifacts.rs` substantially; makes renderers unit-testable (pure
  `Vec<Line>` producers).
- Negative: a breaking change to the `Tool` trait signature (`call`) and the
  `AgentResponse` event set, requiring migration of every built-in tool and the
  provider text-fallback path.
- Migration is sequenced so each step is independently shippable and
  behavior-preserving: (1) `RenderCtx` pure refactor; (2) semantic palette
  layer; (3) `ToolOutput` with `From<String>` back-compat; (4) header grammar +
  breathing dot; (5) content-adaptive bodies; (6) real diff; (7) bash streaming;
  (8) density mode + overlay, deleting per-card `expanded`; (9) `task` inline
  card.

## References

- Prior art studied: `../opencode` (inline→block upgrade, adaptive sibling
  margins, auto split/unified diff at width 120, per-tool icon vocabulary,
  generic-output opt-in) and `../codex` (single bullet + tree gutters, bounded
  head+tail output with `Ctrl+T` transcript overlay, shimmering status dot,
  structured `FileChange`/`CommandOutput`).
- [Built-in tools](../reference/tools.md), [Tool rounds](../explanation/tool-rounds.md).
