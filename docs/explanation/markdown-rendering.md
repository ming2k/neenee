# Markdown rendering

neenee runs its own markdown parser and renderer. This page explains *why*
that decision was made, how the pipeline turns raw provider text into a
semantic document model, and how that model is drawn onto the terminal grid.
For the low-level grid engine underneath, see [Terminal UI](tui.md); for
how the parsed blocks are projected through the renderer, see the
[TUI reference](../reference/tui/).

## Why a custom pipeline

Using an off-the-shelf markdown-to-terminal renderer (e.g. `termimad` or
`pulldown-cmark` → `syntect` → `ratatui::Paragraph`) would produce a
pixel-correct terminal image. That is not what neenee needs. The TUI is a
**structured application**, not a print pipeline: it must support mouse
selection that copies the *original source text*, addressable blocks that
survive reflow, per-cell table hit-testing, and inline-code/bold paint
without disturbing the byte-addressable selection model. A black-box
"markdown → string" pipeline preserves none of that structure.

The custom pipeline therefore does two things that a conventional renderer
cannot:

1.  **Parses markdown into a retained semantic document model** — each
    message is a sequence of `Block` enum variants (Text, Code, Heading,
    ListItem, Quote, Table, Rule, Break) that carry their structured
    properties and preserve the original raw text for copy.
2.  **Renders those blocks into the retained grid** with a layout map that
    records where every block, line, and table cell lands — so a mouse
    coordinate can be resolved back to a document position and a selection
    can be extracted as clean source text.

## The two-path parse

Messages enter the document model through one of two paths, chosen by role:

| Role | Parser | Behaviour |
|------|--------|-----------|
| `Role::User` | `parse_blocks_plain` | The entire text becomes a single `Block::Text`. No markdown interpretation — pasted `# heading` or `` `code` `` syntax stays literal. Line breaks are preserved by the renderer's wrapper. |
| Everything else | `parse_blocks_markdown` | Full markdown parsing: code fences, headings, rules, blockquotes, lists, tables, inline code/bold. |

The split exists so user messages are never structurally mangled. A user
pasting a snippet containing `---` or `| a | b |` does not accidentally
trigger a horizontal rule or table; the transcript stays readable.

## The semantic document model

Each `TranscriptMessage` holds three things:

- `raw` — the original text, preserved byte-for-byte so copy returns
  exactly what the provider emitted.
- `blocks: Vec<Block>` — the parsed semantic structure.
- `kind: MessageKind` — Text, ToolStep, Thinking, or Notice; carries the
  lifecycle state that the step renderer keys off.

### The `Block` enum

| Block | Carries |
|-------|---------|
| `Text` | `content`, plus `code_ranges` and `bold_ranges` for inline paint |
| `Code` | `language` (optional), `content` — the raw fence body |
| `Heading` | `level` (1–6), `content`, inline ranges |
| `ListItem` | `content`, `ordered` (optional number), `depth`, `checked` (task list), inline ranges |
| `Quote` | `content`, inline ranges |
| `Table` | `headers`, `rows`, `aligns`, and a `rendered` pre-rendered grid string |
| `Rule` | (unit variant) |
| `Break` | (unit variant — a blank-line separator inserted by `push_block`) |

### Inline ranges: code and bold as byte spans

Inline code (`` `…` ``) and bold (`**…**`) are *not* stripped from
`content`. Instead the parser records their byte ranges — marker-inclusive
— in `code_ranges` and `bold_ranges`. The renderer paints those spans on a
different colour surface (`code_bg`, `bold` modifier) while the underlying
text stays plain. This is the key invariant that makes **copy return the
original source**: selection extraction walks the plain `content` strings
and never sees the colour markup.

The inline scanner (`scan_inline`) is a single-pass byte-level loop over
the paragraph text. It finds backtick runs for inline code and `**…**`
pairs for bold. Ranges are clamped after `trim_end` so trailing whitespace
removal cannot leave a dangling range past the content boundary.

### The `push_block` gap rule

Every block pushed to the document is routed through `push_block`, which
inserts a `Block::Break` between any two blocks *except* adjacent
`ListItem`s (which stay tight, as in rendered markdown). This means:

- A heading followed by body text gets one blank line.
- A paragraph followed by a code fence gets one blank line.
- Consecutive list items get *no* blank line — they read as a group.

The rule is structural, not cosmetic: the renderer does not decide spacing
ad-hoc; the document model carries it.

## The markdown parser (line-by-line state machine)

`parse_blocks_markdown` walks the input line by line, maintaining a
paragraph accumulator. Each line is classified by its leading pattern:

| Pattern | Dispatches to | Produces |
|---------|--------------|----------|
| `` ``` `` | Fenced code block collector | `Block::Code` |
| `---`, `***`, `___` (≥3) | `is_rule` | `Block::Rule` |
| `#` … `######` | `parse_heading` | `Block::Heading` |
| `> ` | `parse_quote` | consecutive `Block::Quote` lines joined |
| `- ` / `* ` / `+ ` / `N. ` / `- [x]` / `- [ ]` | `parse_list_item` | group of `Block::ListItem` |
| `\| … \|` followed by separator row | `split_table_row` / `TableAccumulator` | `Block::Table` with `rendered` grid |
| Everything else | accumulated as paragraph lines | `Block::Text` (flushed on any block boundary) |

Paragraph accumulation follows standard markdown line-joining rules: a
line ending in two spaces inserts a hard break (`\n`); otherwise adjacent
lines are joined with a space (soft break). When a paragraph is flushed,
`scan_inline` is run over the joined content to record code and bold
ranges.

### Table parsing and pre-rendering

GFM tables (`| header | … |` followed by `| --- | … |` and body rows) are
parsed into a `TableAccumulator`. The accumulator immediately
pre-renders the table into a box-drawing grid string (`┌…┬…┐` / `├…┼…┤` /
`└…┴…┘`) with per-column alignment, stored as `Block::Table::rendered`.

This pre-rendered string serves two purposes:
- **Copy**: extracting a `Table` block returns the aligned grid — clean,
  readable text rather than raw pipe-delimited rows.
- **Parser-level rendering**: gives a fallback rendering for contexts
  that don't go through the full TUI render pipeline.

At draw time, however, the TUI renderer *re-lays-out* the table through
`markdown_table.rs` (see below) for adaptive column sizing on the current
viewport width.

## Rendering to the grid

### The render pipeline

```
parse_blocks()
  └─ markdown text → Vec<Block>

draw_message_body()          [message_body.rs]
  ├─ for each Block:
  │   ├─ Text     → wrap_text() → line_spans_rich() + selection paint
  │   ├─ Code     → code_gutter_line() + language badge
  │   ├─ Heading  → bold weight + level-dependent indent
  │   ├─ ListItem → marker prefix (• / 1. / [x]) + wrapped content
  │   ├─ Quote    → `┃` bar prefix + muted colour
  │   ├─ Table    → build_table_render() [markdown_table.rs] → box-drawing grid
  │   ├─ Rule     → full-width `─` line
  │   └─ Break    → blank row
  └─ for each rendered line:
      ├─ record BlockRegion in LayoutMap
      └─ paint into neenee-tui Grid via Frame
```

`draw_message_body` walks each message's blocks sequentially, tracks the
current Y position, and respects `skip_rows` for scroll offset. Each block
type has its own rendering branch, but they all share:

- `wrap_text()` for width-aware wrapping with CJK kinsoku line-breaking
  (ported from `neenee-tui::text`).
- `line_selection()` / `line_spans_rich()` for painting the selection
  highlight across multi-span lines (code, bold, and plain segments within
  one wrapped line).
- `block_selection_range()` for determining which byte range of a block is
  selected, if any.

### Adaptive table layout (`markdown_table.rs`)

At draw time, tables are re-laid-out to fit the current viewport width.
The layout algorithm:

1.  **Compute intrinsic widths** — each column's width is the widest cell
    (header or body) in that column, measured in Unicode display width.
2.  **Check overflow** — if `sum(widths) + border_overhead > max_width`,
    columns are **proportionally shrunk** by `shrink_column_widths()`,
    with a hard minimum of 3 display columns per column.
3.  **Wrap cell text** — each cell's content is wrapped into multiple
    lines at its allotted column width via `wrap_text()`.
4.  **Build grid lines** — box-drawing borders (`┌┬┐` / `├┼┤` / `└┴┘`)
    are generated with proper column alignment. Each data line records
    per-column byte spans (`TableRowInfo::col_spans`) for hit-testing.
5.  **Record hit-boxes** — the resulting `TableRender` carries per-cell
    byte spans so clicks on a specific table cell can be resolved to a
    `SelectionState::TableCell`.

The column-shrink algorithm distributes the available budget in proportion
to how far each column is above the minimum:

```
above_min[i] = intrinsic[i] - min_col
shrunk[i]   = min_col + above_min[i] * available / sum(above_min)
```

This means a wide column shrinks more than a narrow one, keeping the table
readable rather than collapsing all columns equally.

### Text layout and wrapping (`text_layout.rs`)

`wrap_text()` is the single entry point for all text wrapping in the TUI.
It is Unicode-width-aware (via `unicode_width`) and handles:

- **CJK characters** — width-2 glyphs count as 2 display columns.
- **Kinsoku line-breaking** — inherited from `neenee-tui::text`, prevents
  certain characters from starting or ending a line in CJK text.
- **Code gutter** — `code_gutter_line()` produces the line-number column
  for code blocks, with a `│` separator and muted numbering.
- **Selection arithmetic** — `block_selection_range()`,
  `line_selection()`, and related helpers map between the selection state
  (message index, block index, byte range) and individual wrapped lines.

### The layout map

During rendering, every block and table cell registers its screen position
in the `LayoutMap`:

```
LayoutMap
  ├─ blocks: Vec<BlockRegion>     # (message_idx, block_idx, byte_range, screen_rect)
  └─ table_cells: Vec<TableCellHit>  # (message_idx, block_idx, row, col, screen_rect)
```

This map is the bridge between the semantic document and the screen grid.
It is rebuilt every frame (it is cheap — a few hundred entries) and serves
two directions:

- **Screen → document**: a mouse click at (x, y) is resolved by scanning
  `blocks` and `table_cells` for a containing rectangle.
- **Document → copy**: when the user copies a selection, the selection's
  document range is walked through the block model — `get_selected_text()`
  concatenates the relevant byte slices of each block's content, stripping
  box-drawing borders from table cells.

## Design consequences

### Selection copies the original source

Because `raw` is preserved and blocks carry plain `content` strings with
marker-inclusive inline ranges, `get_selected_text()` returns the original
markdown source — not the terminal-wrapped projection. A code block copies
as its raw source (no line numbers, no gutter), a table cell copies as
clean cell text (no box-drawing characters), and inline code copies with
its backticks intact.

### Tables are hit-testable

The dual table rendering pipeline (parse-time pre-render + draw-time
`build_table_render` with `col_spans`) means every table cell is
individually addressable. Mouse selection within a table enters
`SelectionState::TableCell`, which locks the selection to cell boundaries
and strips borders on copy. See [Table hit-testing](table-hit-testing.md)
for the full design.

### Structure survives terminal resize

Because the document model is independent of screen dimensions, resizing
the terminal only triggers a re-layout — the `Vec<Block>` is unchanged,
selection ranges stay valid, and scroll position is preserved relative to
block boundaries. The renderer simply re-wraps text at the new width.

### Parser is deliberately lightweight

The custom parser handles exactly the markdown subset that LLM providers
emit in practice: fenced code blocks, headings, bold, inline code, lists
(including task lists), blockquotes, tables, and thematic breaks. It
intentionally does **not** handle: nested lists (depth is always 0), HTML
blocks, reference-style links, footnotes, or definition lists. These are
not part of the LLM output surface and would add parsing complexity with
no practical benefit.

The total parser is ~600 lines including tests. It is intentionally a
hand-written state machine rather than a generated parser so that the
inline-range tracking and the `push_block` gap semantics remain
transparent and maintainable.

## Where the code lives

| Concern | File |
|---------|------|
| Document model (`Block`, `TranscriptMessage`, `MessageKind`) | `crates/neenee-code/src/tui/document.rs` |
| Markdown parser (`parse_blocks_markdown`, inline scanner, table accumulator) | `crates/neenee-code/src/tui/document.rs` |
| Message body renderer (`draw_message_body`) | `crates/neenee-code/src/tui/render/message_body.rs` |
| Adaptive table layout (`build_table_render`, `shrink_column_widths`) | `crates/neenee-code/src/tui/render/markdown_table.rs` |
| Text wrapping, CJK, code gutter, selection helpers | `crates/neenee-code/src/tui/render/text_layout.rs` |
| Layout map and hit-testing (`LayoutMap`, `BlockRegion`, `TableCellHit`) | `crates/neenee-code/src/tui/layout.rs` |
| Selection extraction (`get_selected_text`) | `crates/neenee-code/src/tui/selection.rs` |
| Grid engine (`Grid`, `diff`, `Backend`) | `crates/neenee-tui/src/` |
| Export-to-markdown (clipboard handoff) | `crates/neenee-server/src/export.rs` |
