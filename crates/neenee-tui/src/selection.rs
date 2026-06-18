//! Semantic selection manager.
//!
//! Tracks which semantic blocks / text ranges the user has selected.
//! Selection is stored in terms of `SemanticCursor` (message, block, byte offset)
//! so copying always returns the *original* text, not terminal-wrapped output.

use crate::document::{Block, ChatMessage};
use crate::layout::SemanticCursor;
use std::borrow::Cow;

/// Return the original text of one logical table cell. `cell_idx` is row-major
/// (`row * ncols + col`, header is row 0). Falls back to an empty string for
/// out-of-range indices (e.g. a body row with fewer columns than the header).
fn table_cell_text(block: &Block, cell_idx: usize) -> String {
    if let Block::Table {
        headers, rows, ..
    } = block
    {
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
                        '│' | '─'
                            | '┌'
                            | '┐'
                            | '└'
                            | '┘'
                            | '├'
                            | '┤'
                            | '┬'
                            | '┴'
                            | '┼'
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
    /// One logical table cell is selected. `cell_idx` is row-major
    /// (`row * ncols + col`, with the header as row 0) so it maps back to the
    /// block's headers/rows. Selecting a cell grabs its full (possibly
    /// line-wrapped) text without bleeding into adjacent cells or borders.
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
    pub fn is_none(&self) -> bool {
        matches!(self, SelectionState::None)
    }

    pub fn is_active(&self) -> bool {
        !self.is_none()
    }

    /// Start a new range selection at the given cursor.
    pub fn start_range(anchor: SemanticCursor) -> Self {
        SelectionState::Range {
            anchor,
            head: anchor,
        }
    }

    /// Update the head of a range selection.
    pub fn update_head(&mut self, head: SemanticCursor) {
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

    /// Check if a given block is part of the current selection.
    pub fn contains_block(&self, message_idx: usize, block_idx: usize) -> bool {
        match self {
            SelectionState::Block {
                message_idx: mi,
                block_idx: bi,
            } => *mi == message_idx && *bi == block_idx,
            SelectionState::TableCell {
                message_idx: mi,
                block_idx: bi,
                ..
            } => *mi == message_idx && *bi == block_idx,
            SelectionState::Range { anchor, head } => {
                let (start, end) = if anchor <= head {
                    (*anchor, *head)
                } else {
                    (*head, *anchor)
                };
                let target = SemanticCursor::new(message_idx, block_idx, 0);
                // Target block is selected if its cursor falls within the range.
                // For simplicity we treat block granularity: any overlap means selected.
                target >= start && target <= end
            }
            SelectionState::None => false,
        }
    }

    /// Check if a specific byte offset within a block is selected.
    pub fn contains_byte(&self, message_idx: usize, block_idx: usize, byte_offset: usize) -> bool {
        match self {
            SelectionState::Block {
                message_idx: mi,
                block_idx: bi,
            } => *mi == message_idx && *bi == block_idx,
            SelectionState::TableCell { .. } => false,
            SelectionState::Range { anchor, head } => {
                let (start, end) = if anchor <= head {
                    (*anchor, *head)
                } else {
                    (*head, *anchor)
                };
                let cursor = SemanticCursor::new(message_idx, block_idx, byte_offset);
                cursor >= start && cursor <= end
            }
            SelectionState::None => false,
        }
    }
}

/// Extract the selected text from the document model.
///
/// This returns the *original* text data, ignoring any terminal line wrapping.
/// `table_grid` resolves the last-rendered grid for a `Block::Table` so cell
/// selection returns the actually-displayed text with borders stripped.
pub fn get_selected_text<'a>(
    state: &SelectionState,
    messages: &'a [ChatMessage],
    table_grid: &dyn Fn(usize, usize) -> Option<&'a str>,
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
        SelectionState::Range { anchor, head } => {
            let (start, end) = if anchor <= head {
                (*anchor, *head)
            } else {
                (*head, *anchor)
            };

            if start.message_idx == end.message_idx {
                // Selection within a single message.
                let msg = messages.get(start.message_idx)?;
                extract_within_message(
                    start.message_idx,
                    msg,
                    &start,
                    &end,
                    table_grid,
                )
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

/// Snap a byte offset down to the nearest char boundary, clamped to the text.
pub(crate) fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut i = offset.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Return the end (exclusive) of the character that starts at or contains
/// `offset`, so the character under the selection head is included.
pub(crate) fn inclusive_end(text: &str, offset: usize) -> usize {
    let start = floor_char_boundary(text, offset);
    match text[start..].chars().next() {
        Some(ch) => start + ch.len_utf8(),
        None => start,
    }
}

/// Extract text from a single message between two cursors.
///
/// The end cursor is inclusive: the character it points at is part of the
/// selection. All offsets are snapped to char boundaries so multi-byte text
/// can never cause an out-of-boundary slice.
fn extract_within_message<'a>(
    message_idx: usize,
    msg: &'a ChatMessage,
    start: &SemanticCursor,
    end: &SemanticCursor,
    table_grid: &dyn Fn(usize, usize) -> Option<&'a str>,
) -> Option<String> {
    let mut result = String::new();

    for bi in start.block_idx..=end.block_idx.min(msg.blocks.len().saturating_sub(1)) {
        let block = msg.blocks.get(bi)?;
        let (text, strip) = block_copy_text(block, message_idx, bi, table_grid);

        let byte_start = if bi == start.block_idx {
            floor_char_boundary(&text, start.byte_offset)
        } else {
            0
        };
        let byte_end = if bi == end.block_idx {
            inclusive_end(&text, end.byte_offset)
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

/// A helper that manages the lifecycle of a mouse-drag selection.
#[derive(Debug, Default)]
pub struct SelectionDrag {
    pub active: bool,
    pub anchor: Option<SemanticCursor>,
    /// When the drag started inside a table cell, this holds
    /// `(message_idx, block_idx, cell_idx)` so each update can be clamped to
    /// that cell's hit boxes, preventing the selection from crossing `│`
    /// borders into adjacent cells.
    pub cell_constraint: Option<(usize, usize, usize)>,
}

impl SelectionDrag {
    pub fn start(&mut self, cursor: SemanticCursor) {
        self.active = true;
        self.anchor = Some(cursor);
        self.cell_constraint = None;
    }

    /// Start a drag that is confined to a single table cell.
    pub fn start_in_cell(&mut self, cursor: SemanticCursor, cell: (usize, usize, usize)) {
        self.active = true;
        self.anchor = Some(cursor);
        self.cell_constraint = Some(cell);
    }

    pub fn update(&mut self, cursor: SemanticCursor) -> SelectionState {
        match self.anchor {
            Some(anchor) => SelectionState::Range {
                anchor,
                head: cursor,
            },
            None => SelectionState::None,
        }
    }

    pub fn end(&mut self) {
        self.active = false;
        self.cell_constraint = None;
        // Keep anchor so the selection remains; it will be cleared externally.
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.anchor = None;
        self.cell_constraint = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Block, ChatMessage};
    use neenee_core::Role;

    #[test]
    fn test_block_selection() {
        let msg = ChatMessage::new(Role::Assistant, "Hello world");
        let messages = vec![msg];
        let sel = SelectionState::Block {
            message_idx: 0,
            block_idx: 0,
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None),
            Some("Hello world".to_string())
        );
    }

    #[test]
    fn expanded_tool_step_copy_uses_semantic_detail() {
        let mut message = ChatMessage::tool_step("call_1", "read_file", r#"{"path":"README.md"}"#);
        message.finish_tool_step("call_1", "file contents", 42);
        message.set_tool_step_expanded(true);
        // Block 0 = display arguments (key-value text), block 1 = output.
        let arg_index = message
            .blocks
            .iter()
            .position(|block| {
                matches!(block, Block::Text { content } if content.contains("README.md"))
            })
            .unwrap();

        let copied = get_selected_text(
            &SelectionState::Block {
                message_idx: 0,
                block_idx: arg_index,
            },
            &[message],
            &|_, _| None,
        )
        .unwrap();

        assert!(copied.contains("README.md"));
    }

    #[test]
    fn test_range_selection_within_block() {
        let msg = ChatMessage::new(Role::Assistant, "Hello world");
        let messages = vec![msg];
        // Head points at the last char; inclusive semantics select it.
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 6),
            head: SemanticCursor::new(0, 0, 10),
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None),
            Some("world".to_string())
        );
    }

    #[test]
    fn test_range_cross_blocks() {
        let mut msg = ChatMessage::new(Role::Assistant, "");
        msg.blocks = vec![
            Block::Text {
                content: "First".to_string(),
            },
            Block::Text {
                content: "Second".to_string(),
            },
        ];
        let messages = vec![msg];
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 2),
            head: SemanticCursor::new(0, 1, 3),
        };
        // Head at byte 3 ('o') is included.
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None),
            Some("rst\nSeco".to_string())
        );
    }

    #[test]
    fn multibyte_selection_never_panics_and_includes_head_char() {
        let msg = ChatMessage::new(Role::Assistant, "你好世界");
        let messages = vec![msg];

        // Head in the middle of 世 (byte 7 is not a boundary) — must not panic.
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 1),
            head: SemanticCursor::new(0, 0, 7),
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None),
            Some("你好世".to_string())
        );
    }

    #[test]
    fn selection_head_past_text_end_is_clamped() {
        let msg = ChatMessage::new(Role::Assistant, "短文本");
        let messages = vec![msg];
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 0),
            head: SemanticCursor::new(0, 0, 999),
        };
        assert_eq!(
            get_selected_text(&sel, &messages, &|_, _| None),
            Some("短文本".to_string())
        );
    }

    #[test]
    fn table_block_copy_strips_borders() {
        let grid = "┌─────┬──────┐\n│ a   │ b    │\n├─────┼──────┤\n│ c   │ d    │\n└─────┴──────┘";
        let grid_fn = |mi: usize, bi: usize| {
            assert_eq!((mi, bi), (0, 0));
            Some(grid)
        };
        let mut msg = ChatMessage::new(Role::Assistant, "");
        msg.blocks = vec![Block::Table {
            headers: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec!["c".to_string(), "d".to_string()]],
            aligns: vec![crate::document::TableAlignment::None; 2],
            rendered: String::new(),
        }];
        let messages = vec![msg];
        let sel = SelectionState::Block {
            message_idx: 0,
            block_idx: 0,
        };
        let copied = get_selected_text(&sel, &messages, &grid_fn).unwrap();
        assert_eq!(copied, "a b\nc d");
    }

    #[test]
    fn table_range_copy_strips_borders() {
        // Whole first data line selected via byte range over the grid.
        let grid = "┌─────┬──────┐\n│ hello │ world │\n└─────┴──────┘";
        let grid_fn = |_: usize, _: usize| Some(grid);
        let mut msg = ChatMessage::new(Role::Assistant, "");
        msg.blocks = vec![Block::Table {
            headers: vec!["hello".to_string(), "world".to_string()],
            rows: vec![],
            aligns: vec![crate::document::TableAlignment::None; 2],
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
        let copied = get_selected_text(&sel, &messages, &grid_fn).unwrap();
        assert_eq!(copied, "hello world");
    }

    #[test]
    fn table_cell_copy_returns_cell_text() {
        // header row 0: "name" (cell 0), "value" (cell 1)
        // body  row 1: "a"    (cell 2), "b"     (cell 3)
        let mut msg = ChatMessage::new(Role::Assistant, "");
        msg.blocks = vec![Block::Table {
            headers: vec!["name".to_string(), "value".to_string()],
            rows: vec![vec!["a".to_string(), "b".to_string()]],
            aligns: vec![crate::document::TableAlignment::None; 2],
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
        let mut msg = ChatMessage::new(Role::Assistant, "");
        let long = "the quick brown fox jumps over the lazy dog";
        msg.blocks = vec![Block::Table {
            headers: vec!["desc".to_string()],
            rows: vec![vec![long.to_string()]],
            aligns: vec![crate::document::TableAlignment::None],
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
        )
        .unwrap();
        assert_eq!(copied, long);
    }
}
