# Table hit-testing and cell-locked selection

neenee renders GFM tables as Unicode box-drawing grids. A table is not a
flat paragraph — it has internal structure (cells, borders, alignment) that
the selection system must respect. This page traces the pipeline from
parsed `Block::Table` through layout, hit registration, click resolution,
and copy, explaining **why** tables get a second, parallel hit-test system
alongside the generic `BlockRegion` one.

## The problem in one sentence

A click inside `│ hello │ world │` must resolve to _one_ cell — not the
whole line — and a drag must never cross a `│` border into the adjacent
cell. The generic line-based hit-test can't do this; it only sees the full
grid line as one region.

---

## Phase 1 — Parsing (document.rs)

`parse_blocks_markdown()` in
[`document.rs`](../../crates/neenee-code/src/tui/document.rs) detects a
table when it sees `| ... |` followed by a separator row (`|---|---|`):

```text
| Name  | Count |
|-------|-------|
| read  | 1     |
| fetch | 250   |
```

It emits a `Block::Table` carrying three fields:

| Field | Type | Example |
|-------|------|---------|
| `headers` | `Vec<String>` | `["Name", "Count"]` |
| `rows` | `Vec<Vec<String>>` | `[["read","1"],["fetch","250"]]` |
| `aligns` | `Vec<TableAlignment>` | `[Left, Right]` |

A width-independent `rendered` string (borders + padding, hardcoded widths)
is also stored on the block so the table can be copied as plain text without
needing the layout engine. The renderer replaces this with a viewport-fitted
grid at draw time.

---

## Phase 2 — Layout (markdown_table.rs)

`build_table_render()` in
[`markdown_table.rs`](../../crates/neenee-code/src/tui/render/markdown_table.rs)
is a **pure function**: it takes `(headers, rows, aligns, max_width)` and
returns a `TableRender` — a set of grid lines plus, for each data line, the
byte span of every cell's padded content within that line.

### Column sizing

1. **Intrinsic width** = widest cell (header or body) in each column, in
   Unicode display columns.
2. If `intrinsic + border_overhead > max_width`, columns shrink
   proportionally via `shrink_column_widths()`, with a floor of **3 columns
   per cell**.
3. Each cell text is wrapped into its column width via `wrap_text()`, so a
   cell may occupy multiple screen lines.

### Grid assembly

The function builds lines like:

```text
┌───────┬───────┐      ← top border
│ Name  │ Count │      ← header data row
├───────┼───────┤      ← separator
│ read  │ 1     │      ← body row 0
│ fetch │ 250   │      ← body row 1
└───────┴───────┘      ← bottom border
```

Each data line returns a `TableRowInfo`:

```rust
pub(super) struct TableRowInfo {
    pub row: usize,                           // 0=header, 1+=body
    pub col_spans: Vec<(usize, usize)>,       // (byte_start, byte_end) per cell
}
```

The `col_spans` are the **bridge** between layout and hit-testing: they tell
the renderer exactly where each cell's padded text lives inside `│ Name  │
Count │`.

### Alignment

`pad_cell_text()` handles `Left` / `Right` / `Center` / `None` alignment by
inserting spaces to reach the column width.

---

## Phase 3 — Rendering and hit registration (message_body.rs)

The `Block::Table` branch of `draw_message_body()` in
[`message_body.rs`](../../crates/neenee-code/src/tui/render/message_body.rs)
does three things for every frame:

### 3a. Call the layout engine

```rust
let table = build_table_render(headers, rows, aligns, available_width);
```

### 3b. Register the full-grid text

```rust
layout_map.record_table_grid(mi, bi, table.lines.join("\n"));
```

This is used by whole-table copy (middle-click) and by range-selection copy
within a table block — the `strip_table_borders()` function filters out
box-drawing characters from the result.

### 3c. Register per-cell hit boxes

For each data line, the renderer walks `col_spans` and registers a
`TableCellHit` for every cell:

```rust
let col_start = line_text[..lo].width();   // display columns before this cell
let col_w     = line_text[lo..hi].width(); // display columns of this cell
let rect = Rect::new(
    area.x + indent as u16 + col_start as u16,
    *current_y,
    col_w as u16,
    1,
);
layout_map.push_table_cell_hit(TableCellHit {
    message_idx: mi,
    block_idx: bi,
    cell_idx: info.row * ncols + ci,       // row-major, header = row 0
    rect,
});
```

Data lines **also** register a regular `BlockRegion` covering the full line
(used for range selection, cursor navigation). Border lines register
nothing — they are dead zones.

After a frame, `LayoutMap` contains **two overlapping coordinate systems**
for the same cells:

| System | Recorded as | Granularity | Used for |
|--------|-------------|-------------|----------|
| Block regions | `Vec<BlockRegion>` | Full line | Semantic cursor, range selection, keyboard nav |
| Cell hit boxes | `Vec<TableCellHit>` | Individual cell | Click→cell resolution, cell-locked drag |

---

## Phase 4 — The dual hit-test system (layout.rs)

[`layout.rs`](../../crates/neenee-code/src/tui/layout.rs) exposes two
query methods on `LayoutMap`:

### `hit_test(x, y) → Option<SemanticCursor>`

Walks `regions`, finds the one whose `Rect` contains `(x, y)`, then maps the
column offset to a byte offset using Unicode width accounting (CJK-aware).
Returns a `SemanticCursor { message_idx, block_idx, byte_offset }`.

This is the **general-purpose** cursor resolver used by all content types.

### `table_cell_at(x, y) → Option<(usize, usize, usize)>`

Walks `table_cell_hits` and checks `Rect` containment. Returns
`(message_idx, block_idx, cell_idx)` where `cell_idx = row * ncols + col`.

This is **table-specific** — only `event_loop.rs` calls it, and only after
the general cursor resolution confirms the click is not on a step summary
or the input box.

A click on a table data cell **passes both queries**: `hit_test` returns a
cursor somewhere inside the line, and `table_cell_at` returns the specific
cell. The two results serve different purposes in the click cascade.

---

## Phase 5 — The click cascade (event_loop.rs)

When a mouse click arrives at `(x, y)`, the event loop runs a priority
chain in
[`event_loop.rs`](../../crates/neenee-code/src/tui/event_loop.rs):

```text
┌─ Input box?            → focus Compose, start text selection
├─ Step summary?         → navigate envoy / toggle step
└─ Content region?       → resolve cursor, then:
    ├─ table_cell_at(x,y) hits?  → drag.start_in_cell()  [cell-locked]
    └─ otherwise                 → drag.start(cursor)     [free text drag]
```

**The important detail**: a plain click on a cell does **not** immediately
set `SelectionState::TableCell`. Instead it:

1. Clears any existing selection.
2. Arms a `SelectionDrag` with `origin_cell = Some((mi, bi, cell))`.
3. Waits for the next `SelectionUpdate` (pointer movement).

Only when the pointer actually moves does the drag resolve to
`SelectionState::TableCell { ... }`. This means a stationary click on a
cell has no side effect other than switching focus to Browse — the cell
is never highlighted until the user drags.

---

## Phase 6 — The cell-locked drag (selection.rs)

[`selection.rs`](../../crates/neenee-code/src/tui/selection.rs) defines:

```rust
pub struct SelectionDrag {
    pub active: bool,
    pub anchor: Option<SemanticCursor>,
    /// If Some, the drag is locked to this cell regardless of pointer position.
    pub origin_cell: Option<(usize, usize, usize)>,
}
```

Two entry points:

| Method | Sets `origin_cell` | Behaviour on drag |
|--------|--------------------|-------------------|
| `start(cursor)` | `None` | Selection follows pointer (free text range) |
| `start_in_cell(cursor, cell)` | `Some(cell)` | Selection stays pinned to `cell` forever |

On every `SelectionUpdate` event:

```rust
if let Some((mi, bi, cell)) = app.drag.origin_cell {
    // Locked: pointer can move anywhere — even out of the table —
    // but selection remains TableCell { mi, bi, cell }.
    app.selection = SelectionState::TableCell { message_idx: mi, block_idx: bi, cell_idx: cell };
} else {
    // Free: head cursor follows pointer.
    app.selection.update_head(cursor);
}
```

This is the mechanism that guarantees a cell drag **never crosses `│`
borders** into an adjacent cell. The pointer might wander across the screen,
but the selection stays pinned to the origin cell.

---

## Phase 7 — Copy (selection.rs)

When the user copies, `get_selected_text()` routes through the selection
variant:

### `SelectionState::TableCell`

Calls `table_cell_text(block, cell_idx)`, which directly indexes into
`headers[col]` or `rows[row-1][col]` — the **original** cell text,
unaffected by wrapping or terminal width.

### `SelectionState::Block` or `Range` on a table block

Uses `block_copy_text()`, which prefers the last-rendered grid from
`LayoutMap.table_grids` (viewport-adapted) over the block's stored
`rendered` field. Either way, the result is passed through
`strip_table_borders()`:

```rust
fn strip_table_borders(s: &str) -> String {
    // Filter out │─┌┐└┘├┤┬┴┼
    // Collapse multiple spaces into one
    // Drop empty lines (pure border rows)
}
```

The output is clean cell text with one space between columns and no
box-drawing artifacts.

---

## Why two systems?

The generic `BlockRegion` / `hit_test` system works perfectly for prose,
code, headings, quotes, and lists — one line = one region. But a table
grid line is a composite: `│` borders, inter-cell padding, and multiple
cells of content all on one screen row. A single `BlockRegion` covering
the whole line can't tell which cell a click landed in.

The alternatives considered:

| Approach | Verdict |
|----------|---------|
| Sub-divide `BlockRegion` per cell | Would force every block type to carry cell semantics; breaks the uniform cursor model. |
| Mark cell boundaries in `BlockRegion.text` with sentinel bytes | Fragile — sentinels can collide with real content, complicate copy, and don't survive wrapping. |
| Separate `table_cell_hits` vector (chosen) | Clean separation: the generic system stays generic, tables add their own hit layer alongside it. |

The cost is that `event_loop.rs` must check `table_cell_at` after
`resolve_cursor` — an extra branch in the click cascade. The benefit is that
no other block type pays for table complexity, and table-specific behaviour
(cell locking, border stripping) stays in modules that already know about
table structure.

---

## Source file map

| File | Role |
|------|------|
| [`document.rs`](../../crates/neenee-code/src/tui/document.rs) `:1325-1358` | Parses GFM tables into `Block::Table` |
| [`markdown_table.rs`](../../crates/neenee-code/src/tui/render/markdown_table.rs) | Pure layout: column sizing, wrapping, grid assembly, `col_spans` |
| [`message_body.rs`](../../crates/neenee-code/src/tui/render/message_body.rs) `:305-492` | Renders table lines, registers `TableCellHit`s and `table_grid` |
| [`layout.rs`](../../crates/neenee-code/src/tui/layout.rs) `:83-167` | `LayoutMap` with dual hit systems: `hit_test` + `table_cell_at` |
| [`selection.rs`](../../crates/neenee-code/src/tui/selection.rs) `:88-113,306-347` | `SelectionState::TableCell`, `SelectionDrag.origin_cell`, `table_cell_text`, `strip_table_borders` |
| [`event_loop.rs`](../../crates/neenee-code/src/tui/event_loop.rs) `:2354-2432` | Click cascade: `table_cell_at` check, cell-locked drag arm/dispatch |
