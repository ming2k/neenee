//! Semantic selection manager.
//!
//! Tracks which semantic blocks / text ranges the user has selected.
//! Selection is stored in terms of `SemanticCursor` (message, block, byte offset)
//! so copying always returns the *original* text, not terminal-wrapped output.

use crate::tui::document::{Block, TranscriptMessage};
use crate::tui::layout::{LayoutMap, SemanticCursor, TableCellSegment};
use std::borrow::Cow;

/// Re-exported grapheme-boundary primitives so the rest of the crate keeps the
/// existing `crate::tui::selection::…` import path. The implementations live in
/// the engine ([`neenee_tui::text`]), the single owner of grapheme/width
/// measurement.
pub(crate) use neenee_tui::text::{floor_grapheme_boundary, inclusive_grapheme_end};

/// Return the original text of one logical table cell. `cell_idx` is row-major
/// (`row * ncols + col`, header is row 0). Falls back to an empty string for
/// out-of-range indices (e.g. a body row with fewer columns than the header).
fn table_cell_text(block: &Block, cell_idx: usize) -> String {
    if let Block::Table { headers, rows, .. } = block {
        let ncols = headers.len().max(1);
        let row = cell_idx / ncols;
        let col = cell_idx % ncols;
        if row == 0 {
            return headers.get(col).cloned().unwrap_or_default();
        }
        return rows
            .get(row - 1)
            .and_then(|r| r.get(col))
            .cloned()
            .unwrap_or_default();
    }
    String::new()
}

/// Resolve the copyable text for a block. For `Block::Table` we prefer the
/// last-rendered grid (which reflects viewport reshaping) and strip the
/// box-drawing borders so the result is clean cell text; otherwise we use
/// the block's own raw text. Returns `(text, strip_borders)`.
fn block_copy_text<'a>(
    block: &'a Block,
    message_idx: usize,
    block_idx: usize,
    table_grid: &dyn Fn(usize, usize) -> Option<&'a str>,
) -> (Cow<'a, str>, bool) {
    if let Block::Table { .. } = block {
        if let Some(grid) = table_grid(message_idx, block_idx) {
            return (Cow::Borrowed(grid), true);
        }
        // Fall back to the width-independent grid stored on the block.
        return (Cow::Borrowed(block.raw_text()), true);
    }
    (Cow::Borrowed(block.raw_text()), false)
}

/// Strip box-drawing borders/padding from a rendered-table slice so only the
/// cell text remains. Newlines are preserved so multi-row selections keep row
/// breaks; within each line runs of whitespace collapse to a single space.
/// Pure-border lines (and trailing-newline artifacts) become empty and are
/// dropped so the result has no blank lines.
fn strip_table_borders(s: &str) -> String {
    s.split('\n')
        .map(|line| {
            let stripped: String = line
                .chars()
                .filter(|c| {
                    !matches!(
                        c,
                        '│' | '─' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼'
                    )
                })
                .collect();
            let mut out = String::with_capacity(stripped.len());
            let mut last_space = true;
            for ch in stripped.chars() {
                if ch.is_whitespace() {
                    if !last_space {
                        out.push(' ');
                        last_space = true;
                    }
                } else {
                    out.push(ch);
                    last_space = false;
                }
            }
            out.trim_end().to_string()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The user's current selection state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SelectionState {
    /// Nothing selected.
    #[default]
    None,
    /// A single block is fully selected (e.g. triple-click on a code block).
    Block {
        message_idx: usize,
        block_idx: usize,
    },
    /// One logical table cell is selected in full. `cell_idx` is row-major
    /// (`row * ncols + col`, with the header as row 0) so it maps back to the
    /// block's headers/rows.
    ///
    /// Currently only reachable via copy of a previously set selection;
    /// cell-bounded drags use [`SelectionState::Range`] with
    /// [`CellDragInfo`] clamping instead.
    #[allow(dead_code)]
    TableCell {
        message_idx: usize,
        block_idx: usize,
        cell_idx: usize,
    },
    /// A range spanning from anchor to head (inclusive).
    Range {
        anchor: SemanticCursor,
        head: SemanticCursor,
    },
}

impl SelectionState {
    /// Start a new range selection at the given cursor.
    pub fn start_range(anchor: SemanticCursor) -> Self {
        SelectionState::Range {
            anchor,
            head: anchor,
        }
    }

    /// Update the head of a range selection.
    fn update_head(&mut self, head: SemanticCursor) {
        if let SelectionState::Range { .. } = self {
            *self = SelectionState::Range {
                anchor: match self {
                    SelectionState::Range { anchor, .. } => *anchor,
                    _ => head,
                },
                head,
            };
        }
    }

    /// Normalize the range so that start <= end.
    ///
    /// This preserves collapsed ranges because they are meaningful while a
    /// mouse drag is armed. Use [`Self::active_normalized_range`] when callers
    /// need a visible/copyable range.
    pub fn normalized_range(&self) -> Option<(SemanticCursor, SemanticCursor)> {
        match self {
            SelectionState::Range { anchor, head } => {
                if anchor <= head {
                    Some((*anchor, *head))
                } else {
                    Some((*head, *anchor))
                }
            }
            SelectionState::Block {
                message_idx,
                block_idx,
            } => Some((
                SemanticCursor::new(*message_idx, *block_idx, 0),
                SemanticCursor::new(*message_idx, *block_idx, usize::MAX),
            )),
            SelectionState::TableCell { .. } | SelectionState::None => None,
        }
    }

    /// Normalize only visible/copyable selections. Collapsed ranges are a drag
    /// anchor, not selected content, so they deliberately return `None`.
    pub fn active_normalized_range(&self) -> Option<(SemanticCursor, SemanticCursor)> {
        if !self.is_active() {
            return None;
        }
        self.normalized_range()
    }

    /// Whether the state represents a visible/copyable selection rather than
    /// merely an armed caret. A collapsed range (`anchor == head`) is how mouse
    /// down starts a drag, but it must not paint selection, copy text, or leave
    /// the terminal block cursor visible over the input.
    pub fn is_active(&self) -> bool {
        match self {
            SelectionState::None => false,
            SelectionState::Range { anchor, head } => anchor != head,
            SelectionState::Block { .. } | SelectionState::TableCell { .. } => true,
        }
    }
}

/// Extract the selected text from the document model.
///
/// This returns the *original* text data, ignoring any terminal line wrapping.
/// `table_grid` resolves the last-rendered grid for a `Block::Table` so cell
/// selection returns the actually-displayed text with borders stripped.
/// `cell_info` provides context when the selection is a `Range` bounded
/// inside a table cell (free substring selection within `│` fences).
pub fn get_selected_text<'a>(
    state: &SelectionState,
    messages: &'a [TranscriptMessage],
    table_grid: &dyn Fn(usize, usize) -> Option<&'a str>,
    cell_info: Option<&CellDragInfo>,
) -> Option<String> {
    match state {
        SelectionState::None => None,
        SelectionState::TableCell {
            message_idx,
            block_idx,
            cell_idx,
        } => {
            let msg = messages.get(*message_idx)?;
            let block = msg.blocks.get(*block_idx)?;
            Some(table_cell_text(block, *cell_idx))
        }
        SelectionState::Block {
            message_idx,
            block_idx,
        } => {
            let msg = messages.get(*message_idx)?;
            let block = msg.blocks.get(*block_idx)?;
            let (text, strip) = block_copy_text(block, *message_idx, *block_idx, table_grid);
            Some(if strip {
                strip_table_borders(&text)
            } else {
                text.into_owned()
            })
        }
        SelectionState::Range { .. } => {
            let (start, end) = state.active_normalized_range()?;

            if start.message_idx == end.message_idx {
                // Cell-bounded range: extract a substring of the cell's
                // original text, mapping grid-line byte offsets back.
                if let Some(ci) = cell_info {
                    return Some(ci.extract_range_text(start.byte_offset, end.byte_offset));
                }
                // Selection within a single message.
                let msg = messages.get(start.message_idx)?;
                extract_within_message(start.message_idx, msg, &start, &end, table_grid)
            } else {
                // Selection spans multiple messages.
                let mut result = String::new();
                for mi in start.message_idx..=end.message_idx {
                    let msg = messages.get(mi)?;
                    if mi == start.message_idx {
                        // From start cursor to end of message.
                        let end_cursor =
                            SemanticCursor::new(mi, msg.blocks.len().saturating_sub(1), usize::MAX);
                        if let Some(s) =
                            extract_within_message(mi, msg, &start, &end_cursor, table_grid)
                        {
                            result.push_str(&s);
                        }
                    } else if mi == end.message_idx {
                        // From start of message to end cursor.
                        let start_cursor = SemanticCursor::new(mi, 0, 0);
                        if let Some(s) =
                            extract_within_message(mi, msg, &start_cursor, &end, table_grid)
                        {
                            result.push_str(&s);
                        }
                    } else {
                        // Whole message.
                        result.push_str(&msg.raw);
                    }
                    if mi != end.message_idx {
                        result.push('\n');
                    }
                }
                Some(result)
            }
        }
    }
}

/// Extract text from a single message between two cursors.
///
/// The end cursor is inclusive: the character it points at is part of the
/// selection. All offsets are snapped to grapheme boundaries so multi-byte text
/// and multi-codepoint glyphs can never cause an out-of-boundary slice.
fn extract_within_message<'a>(
    message_idx: usize,
    msg: &'a TranscriptMessage,
    start: &SemanticCursor,
    end: &SemanticCursor,
    table_grid: &dyn Fn(usize, usize) -> Option<&'a str>,
) -> Option<String> {
    let mut result = String::new();

    for bi in start.block_idx..=end.block_idx.min(msg.blocks.len().saturating_sub(1)) {
        let block = msg.blocks.get(bi)?;
        let (text, strip) = block_copy_text(block, message_idx, bi, table_grid);

        let byte_start = if bi == start.block_idx {
            floor_grapheme_boundary(&text, start.byte_offset)
        } else {
            0
        };
        let byte_end = if bi == end.block_idx {
            inclusive_grapheme_end(&text, end.byte_offset)
        } else {
            text.len()
        };

        if byte_start < byte_end {
            let slice = &text[byte_start..byte_end];
            if strip {
                result.push_str(&strip_table_borders(slice));
            } else {
                result.push_str(slice);
            }
        }

        // Add separator between blocks unless at the end.
        if bi < end.block_idx && bi < msg.blocks.len().saturating_sub(1) {
            result.push('\n');
        }
    }

    Some(result)
}

/// Context for a drag that started inside a table cell. Stored alongside
/// [`SelectionDrag`] so the drag can clamp cursor positions to the cell's
/// `│` boundaries without auto-selecting the whole cell.
#[derive(Debug, Clone)]
pub struct CellDragInfo {
    pub message_idx: usize,
    pub block_idx: usize,
    /// Original cell text (from headers/rows, before padding/wrapping).
    pub cell_text: String,
    /// Render/source mappings for each visible wrapped line of this logical
    /// cell. Selection highlighting stays in rendered table byte space; copy
    /// maps those byte offsets back through these segments.
    pub segments: Vec<TableCellSegment>,
}

impl CellDragInfo {
    /// Clamp a cursor's byte offset into the cell's rendered text content. If
    /// the pointer lands in alignment padding or outside this logical cell, it
    /// resolves to the nearest content boundary.
    pub fn clamp_cursor(&self, cursor: SemanticCursor) -> SemanticCursor {
        SemanticCursor::new(
            self.message_idx,
            self.block_idx,
            self.clamp_rendered_offset(cursor.byte_offset),
        )
    }

    /// Extract the selected substring from the cell's original text.
    /// `anchor` and `head` are the clamped cursors (both should be
    /// message_idx == self.message_idx, block_idx == self.block_idx).
    pub fn extract_range_text(&self, start_byte: usize, end_byte: usize) -> String {
        let source_start = self.source_offset_for_rendered(start_byte);
        let source_end = self.source_offset_for_rendered(end_byte);
        let cell_start = floor_grapheme_boundary(&self.cell_text, source_start);
        let cell_end = inclusive_grapheme_end(&self.cell_text, source_end);
        if cell_start < cell_end {
            self.cell_text[cell_start..cell_end].to_string()
        } else {
            String::new()
        }
    }

    fn sorted_segments(&self) -> Vec<TableCellSegment> {
        let mut segments = self.segments.clone();
        segments.sort_by_key(|seg| (seg.content_range.0, seg.content_range.1));
        segments
    }

    fn clamp_rendered_offset(&self, byte_offset: usize) -> usize {
        let segments = self.sorted_segments();
        let Some(first) = segments.first() else {
            return 0;
        };
        if byte_offset <= first.content_range.0 {
            return first.content_range.0;
        }

        let mut previous: Option<TableCellSegment> = None;
        for seg in &segments {
            let (render_lo, render_hi) = seg.rendered_range;
            let (content_lo, content_hi) = seg.content_range;
            if byte_offset < render_lo {
                if let Some(prev) = previous {
                    let prev_distance = byte_offset.saturating_sub(prev.rendered_range.1);
                    let next_distance = render_lo.saturating_sub(byte_offset);
                    return if prev_distance <= next_distance {
                        prev.content_range.1
                    } else {
                        content_lo
                    };
                }
                return content_lo;
            }
            if byte_offset <= render_hi {
                return byte_offset.clamp(content_lo, content_hi);
            }
            previous = Some(*seg);
        }

        segments
            .last()
            .map(|seg| seg.content_range.1)
            .unwrap_or(first.content_range.0)
    }

    fn source_offset_for_rendered(&self, byte_offset: usize) -> usize {
        let segments = self.sorted_segments();
        let Some(first) = segments.first() else {
            return 0;
        };
        if byte_offset <= first.content_range.0 {
            return first.source_range.0.min(self.cell_text.len());
        }

        for seg in &segments {
            let (content_lo, content_hi) = seg.content_range;
            let (source_lo, source_hi) = seg.source_range;
            if byte_offset <= content_hi {
                let in_segment = byte_offset.saturating_sub(content_lo);
                return (source_lo + in_segment)
                    .min(source_hi)
                    .min(self.cell_text.len());
            }
        }

        segments
            .last()
            .map(|seg| seg.source_range.1.min(self.cell_text.len()))
            .unwrap_or(0)
    }
}

/// A helper that manages the lifecycle of a mouse-drag selection.
#[derive(Debug, Default)]
pub struct SelectionDrag {
    pub active: bool,
    pub anchor: Option<SemanticCursor>,
    /// When a drag begins inside a table cell, this stores the cell context
    /// so the selection is clamped to the cell's `│` boundaries. The user
    /// can select a *substring* of the cell — not the whole cell — but the
    /// selection can never cross `│` into an adjacent cell. `None` for
    /// ordinary text-range drags, which follow the pointer freely.
    pub cell_info: Option<CellDragInfo>,
}

impl SelectionDrag {
    /// Arm a text-range drag. The selection follows the pointer (head cursor).
    pub fn start(&mut self, cursor: SemanticCursor) {
        self.active = true;
        self.anchor = Some(cursor);
        self.cell_info = None;
    }

    /// Arm a text-range drag and install the matching collapsed range in the
    /// selection state. The range becomes active only after a later update moves
    /// the head away from the anchor.
    pub fn begin_range(&mut self, selection: &mut SelectionState, cursor: SemanticCursor) {
        self.start(cursor);
        *selection = SelectionState::start_range(cursor);
    }

    /// Arm a drag locked to one table cell's `│` boundaries.
    fn start_in_cell(&mut self, cursor: SemanticCursor, cell: CellDragInfo) {
        self.active = true;
        self.anchor = Some(cursor);
        self.cell_info = Some(cell);
    }

    /// Arm a cell-bounded drag and install a collapsed range clamped to the
    /// originating cell. Cell drags use the same Range lifecycle as ordinary
    /// text drags; the only difference is that updates are clamped.
    pub fn begin_cell(
        &mut self,
        selection: &mut SelectionState,
        cursor: SemanticCursor,
        cell: CellDragInfo,
    ) {
        let anchor = cell.clamp_cursor(cursor);
        self.start_in_cell(anchor, cell);
        *selection = SelectionState::start_range(anchor);
    }

    /// Extend the current drag to a semantic cursor.
    pub fn update_to_cursor(&self, selection: &mut SelectionState, cursor: SemanticCursor) {
        let Some(anchor) = self.anchor else {
            return;
        };
        if !matches!(selection, SelectionState::Range { .. }) {
            *selection = SelectionState::start_range(anchor);
        }
        let head = self
            .cell_info
            .as_ref()
            .map_or(cursor, |cell| cell.clamp_cursor(cursor));
        selection.update_head(head);
    }

    /// Resolve a screen point through the frame layout and extend the drag if
    /// the point maps to selectable text.
    pub fn update_from_point(
        &self,
        selection: &mut SelectionState,
        layout_map: &LayoutMap,
        x: u16,
        y: u16,
    ) {
        if let Some(cursor) = layout_map.cursor_at(x, y) {
            self.update_to_cursor(selection, cursor);
        }
    }

    /// Mark the low-level mouse drag as released. This is used by the input
    /// event translator before the event loop applies semantic cleanup.
    pub fn end(&mut self) {
        self.active = false;
    }

    /// Finish a semantic drag. Collapsed ranges are cleared; active ranges keep
    /// their anchor and optional cell context so copy can resolve them.
    pub fn finish(&mut self, selection: &mut SelectionState) {
        self.active = false;
        if selection.is_active() {
            // Keep anchor + cell_info so a completed cell drag can still copy
            // through the cell-text mapping.
        } else {
            *selection = SelectionState::None;
            self.anchor = None;
            self.cell_info = None;
        }
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.anchor = None;
        self.cell_info = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::document::{Block, TranscriptMessage};
    use neenee_core::Role;

    #[test]
    fn test_block_selection() {
        let msg = TranscriptMessage::new(Role::Assistant, "Hello world");
        let messages = vec![msg];
        let sel = SelectionState::Block {
            message_idx: 0,
            block_idx: 0,
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None, None),
            Some("Hello world".to_string())
        );
    }

    #[test]
    fn active_selection_excludes_collapsed_range() {
        let cursor = SemanticCursor::new(0, 0, 0);
        assert!(!SelectionState::None.is_active());
        assert!(!SelectionState::start_range(cursor).is_active());
        assert_eq!(
            SelectionState::start_range(cursor).active_normalized_range(),
            None
        );
        assert!(
            SelectionState::Range {
                anchor: cursor,
                head: SemanticCursor::new(0, 0, 1),
            }
            .is_active()
        );
        assert!(
            SelectionState::Block {
                message_idx: 0,
                block_idx: 0,
            }
            .is_active()
        );
    }

    #[test]
    fn range_drag_lifecycle_is_collapsed_until_head_moves() {
        let anchor = SemanticCursor::new(0, 0, 0);
        let head = SemanticCursor::new(0, 0, 1);
        let mut drag = SelectionDrag::default();
        let mut selection = SelectionState::None;

        drag.begin_range(&mut selection, anchor);
        assert!(drag.active);
        assert_eq!(selection, SelectionState::start_range(anchor));
        assert!(!selection.is_active());

        drag.update_to_cursor(&mut selection, head);
        assert!(selection.is_active());
        assert_eq!(selection.active_normalized_range(), Some((anchor, head)));

        drag.finish(&mut selection);
        assert!(!drag.active);
        assert_eq!(drag.anchor, Some(anchor));
        assert_eq!(selection.active_normalized_range(), Some((anchor, head)));
    }

    #[test]
    fn collapsed_drag_finish_clears_selection_and_context() {
        let anchor = SemanticCursor::new(0, 0, 0);
        let mut drag = SelectionDrag::default();
        let mut selection = SelectionState::None;

        drag.begin_range(&mut selection, anchor);
        drag.finish(&mut selection);

        assert_eq!(selection, SelectionState::None);
        assert!(!drag.active);
        assert_eq!(drag.anchor, None);
        assert!(drag.cell_info.is_none());
    }

    #[test]
    fn cell_drag_uses_range_lifecycle_and_clamps_updates() {
        let cell = CellDragInfo {
            message_idx: 2,
            block_idx: 3,
            cell_text: "abcdef".to_string(),
            segments: vec![TableCellSegment {
                rendered_range: (8, 18),
                content_range: (10, 16),
                source_range: (0, 6),
            }],
        };
        let mut drag = SelectionDrag::default();
        let mut selection = SelectionState::None;

        drag.begin_cell(&mut selection, SemanticCursor::new(2, 3, 8), cell);
        let anchor = SemanticCursor::new(2, 3, 10);
        assert_eq!(selection, SelectionState::start_range(anchor));
        assert!(!selection.is_active());

        drag.update_to_cursor(&mut selection, SemanticCursor::new(9, 9, 99));
        let head = SemanticCursor::new(2, 3, 16);
        assert_eq!(selection.active_normalized_range(), Some((anchor, head)));
        assert_eq!(
            drag.cell_info.as_ref().unwrap().extract_range_text(10, 16),
            "abcdef"
        );
    }

    #[test]
    fn expanded_tool_step_copy_uses_semantic_detail() {
        let mut message =
            TranscriptMessage::tool_step("call_1", "read_text", r#"{"path":"README.md"}"#);
        message.finish_tool_step(
            "call_1",
            "file contents",
            neenee_core::ToolOutput::text("file contents"),
            42,
        );
        message.set_tool_step_expanded(true);
        // Block 0 = display arguments (key-value text), block 1 = output.
        let arg_index = message
            .blocks
            .iter()
            .position(
                |block| matches!(block, Block::Text { content, .. } if content.contains("README.md")),
            )
            .unwrap();

        let copied = get_selected_text(
            &SelectionState::Block {
                message_idx: 0,
                block_idx: arg_index,
            },
            &[message],
            &|_, _| None,
            None,
        )
        .unwrap();

        assert!(copied.contains("README.md"));
    }

    #[test]
    fn test_range_selection_within_block() {
        let msg = TranscriptMessage::new(Role::Assistant, "Hello world");
        let messages = vec![msg];
        // Head points at the last char; inclusive semantics select it.
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 6),
            head: SemanticCursor::new(0, 0, 10),
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None, None),
            Some("world".to_string())
        );
    }

    #[test]
    fn test_range_cross_blocks() {
        let mut msg = TranscriptMessage::new(Role::Assistant, "");
        msg.blocks = vec![
            Block::Text {
                content: "First".to_string(),
                code_ranges: Vec::new(),
                bold_ranges: Vec::new(),
            },
            Block::Text {
                content: "Second".to_string(),
                code_ranges: Vec::new(),
                bold_ranges: Vec::new(),
            },
        ];
        let messages = vec![msg];
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 2),
            head: SemanticCursor::new(0, 1, 3),
        };
        // Head at byte 3 ('o') is included.
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None, None),
            Some("rst\nSeco".to_string())
        );
    }

    #[test]
    fn multibyte_selection_never_panics_and_includes_head_char() {
        let msg = TranscriptMessage::new(Role::Assistant, "😀😃😄😁");
        let messages = vec![msg];

        // Head in the middle of 😄 (byte 10 is not a boundary) — must not panic.
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 1),
            head: SemanticCursor::new(0, 0, 10),
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None, None),
            Some("😀😃😄".to_string())
        );
    }

    #[test]
    fn selection_head_past_text_end_is_clamped() {
        let msg = TranscriptMessage::new(Role::Assistant, "abc😀");
        let messages = vec![msg];
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 0),
            head: SemanticCursor::new(0, 0, 999),
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None, None),
            Some("abc😀".to_string())
        );
    }

    #[test]
    fn table_block_copy_strips_borders() {
        let grid = "┌─────┬──────┐\n│ a   │ b    │\n├─────┼──────┤\n│ c   │ d    │\n└─────┴──────┘";
        let grid_fn = |mi: usize, bi: usize| {
            assert_eq!((mi, bi), (0, 0));
            Some(grid)
        };
        let mut msg = TranscriptMessage::new(Role::Assistant, "");
        msg.blocks = vec![Block::Table {
            headers: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec!["c".to_string(), "d".to_string()]],
            aligns: vec![crate::tui::document::TableAlignment::None; 2],
            rendered: String::new(),
        }];
        let messages = vec![msg];
        let sel = SelectionState::Block {
            message_idx: 0,
            block_idx: 0,
        };
        let copied = get_selected_text(&sel, &messages, &grid_fn, None).unwrap();
        assert_eq!(copied, "a b\nc d");
    }

    #[test]
    fn table_range_copy_strips_borders() {
        // Whole first data line selected via byte range over the grid.
        let grid = "┌─────┬──────┐\n│ hello │ world │\n└─────┴──────┘";
        let grid_fn = |_: usize, _: usize| Some(grid);
        let mut msg = TranscriptMessage::new(Role::Assistant, "");
        msg.blocks = vec![Block::Table {
            headers: vec!["hello".to_string(), "world".to_string()],
            rows: vec![],
            aligns: vec![crate::tui::document::TableAlignment::None; 2],
            rendered: String::new(),
        }];
        let messages = vec![msg];
        // The data line starts after the first '\n' (byte 13) and runs to the
        // next '\n'. Selecting [13, end_of_line) grabs the whole data row.
        let data_start = grid.find('\n').unwrap() + 1;
        let data_end = grid[data_start..].find('\n').unwrap() + data_start;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, data_start),
            head: SemanticCursor::new(0, 0, data_end),
        };
        let copied = get_selected_text(&sel, &messages, &grid_fn, None).unwrap();
        assert_eq!(copied, "hello world");
    }

    #[test]
    fn table_cell_copy_returns_cell_text() {
        // header row 0: "name" (cell 0), "value" (cell 1)
        // body  row 1: "a"    (cell 2), "b"     (cell 3)
        let mut msg = TranscriptMessage::new(Role::Assistant, "");
        msg.blocks = vec![Block::Table {
            headers: vec!["name".to_string(), "value".to_string()],
            rows: vec![vec!["a".to_string(), "b".to_string()]],
            aligns: vec![crate::tui::document::TableAlignment::None; 2],
            rendered: String::new(),
        }];
        let messages = vec![msg];

        let copy = |cell_idx| {
            get_selected_text(
                &SelectionState::TableCell {
                    message_idx: 0,
                    block_idx: 0,
                    cell_idx,
                },
                &messages,
                &|_, _| None,
                None,
            )
        };
        assert_eq!(copy(0), Some("name".to_string())); // header col 0
        assert_eq!(copy(1), Some("value".to_string())); // header col 1
        assert_eq!(copy(2), Some("a".to_string())); // body row 0, col 0
        assert_eq!(copy(3), Some("b".to_string())); // body row 0, col 1
    }

    #[test]
    fn table_cell_copy_includes_wrapped_text() {
        // A cell whose source text is long (would wrap when rendered) is copied
        // in full — the selection is logical-cell-granular, not line-granular.
        let mut msg = TranscriptMessage::new(Role::Assistant, "");
        let long = "the quick brown fox jumps over the lazy dog";
        msg.blocks = vec![Block::Table {
            headers: vec!["desc".to_string()],
            rows: vec![vec![long.to_string()]],
            aligns: vec![crate::tui::document::TableAlignment::None],
            rendered: String::new(),
        }];
        let messages = vec![msg];
        // cell 0 = header, cell 1 = body (the long text).
        let copied = get_selected_text(
            &SelectionState::TableCell {
                message_idx: 0,
                block_idx: 0,
                cell_idx: 1,
            },
            &messages,
            &|_, _| None,
            None,
        )
        .unwrap();
        assert_eq!(copied, long);
    }

    #[test]
    fn cell_drag_copy_is_grapheme_safe_and_inclusive() {
        let cell = CellDragInfo {
            message_idx: 0,
            block_idx: 0,
            cell_text: "中文测".to_string(),
            segments: vec![TableCellSegment {
                rendered_range: (2, 11),
                content_range: (2, 11),
                source_range: (0, 9),
            }],
        };

        // Offsets are grid-line byte offsets, so subtracting `lo` lands inside
        // the first and third CJK grapheme. Extraction must not panic, and the
        // inclusive head includes the grapheme under the drag head.
        assert_eq!(cell.extract_range_text(3, 8), "中文测");
    }

    #[test]
    fn cell_drag_copy_maps_rendered_padding_to_source_text() {
        let cell = CellDragInfo {
            message_idx: 0,
            block_idx: 0,
            cell_text: "abcdef".to_string(),
            segments: vec![TableCellSegment {
                rendered_range: (20, 30),
                content_range: (22, 28),
                source_range: (0, 6),
            }],
        };

        assert_eq!(
            cell.clamp_cursor(SemanticCursor::new(9, 9, 20)),
            SemanticCursor::new(0, 0, 22)
        );
        assert_eq!(
            cell.clamp_cursor(SemanticCursor::new(9, 9, 29)),
            SemanticCursor::new(0, 0, 28)
        );
        assert_eq!(cell.extract_range_text(22, 24), "abc");
    }

    #[test]
    fn cell_drag_clamps_between_wrapped_segments_to_nearest_boundary() {
        let cell = CellDragInfo {
            message_idx: 0,
            block_idx: 0,
            cell_text: "abcdef".to_string(),
            segments: vec![
                TableCellSegment {
                    rendered_range: (10, 13),
                    content_range: (10, 13),
                    source_range: (0, 3),
                },
                TableCellSegment {
                    rendered_range: (40, 43),
                    content_range: (40, 43),
                    source_range: (3, 6),
                },
            ],
        };

        assert_eq!(
            cell.clamp_cursor(SemanticCursor::new(9, 9, 15)),
            SemanticCursor::new(0, 0, 13)
        );
        assert_eq!(
            cell.clamp_cursor(SemanticCursor::new(9, 9, 39)),
            SemanticCursor::new(0, 0, 40)
        );
    }
}
