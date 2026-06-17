//! Semantic selection manager.
//!
//! Tracks which semantic blocks / text ranges the user has selected.
//! Selection is stored in terms of `SemanticCursor` (message, block, byte offset)
//! so copying always returns the *original* text, not terminal-wrapped output.

use crate::document::ChatMessage;
use crate::layout::SemanticCursor;

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
            SelectionState::None => None,
        }
    }

    /// Check if a given block is part of the current selection.
    pub fn contains_block(&self, message_idx: usize, block_idx: usize) -> bool {
        match self {
            SelectionState::Block {
                message_idx: mi,
                block_idx: bi,
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
pub fn get_selected_text(state: &SelectionState, messages: &[ChatMessage]) -> Option<String> {
    match state {
        SelectionState::None => None,
        SelectionState::Block {
            message_idx,
            block_idx,
        } => {
            let msg = messages.get(*message_idx)?;
            let block = msg.blocks.get(*block_idx)?;
            Some(block.raw_text().to_string())
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
                extract_within_message(msg, &start, &end)
            } else {
                // Selection spans multiple messages.
                let mut result = String::new();
                for mi in start.message_idx..=end.message_idx {
                    let msg = messages.get(mi)?;
                    if mi == start.message_idx {
                        // From start cursor to end of message.
                        let end_cursor =
                            SemanticCursor::new(mi, msg.blocks.len().saturating_sub(1), usize::MAX);
                        if let Some(s) = extract_within_message(msg, &start, &end_cursor) {
                            result.push_str(&s);
                        }
                    } else if mi == end.message_idx {
                        // From start of message to end cursor.
                        let start_cursor = SemanticCursor::new(mi, 0, 0);
                        if let Some(s) = extract_within_message(msg, &start_cursor, &end) {
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
fn extract_within_message(
    msg: &ChatMessage,
    start: &SemanticCursor,
    end: &SemanticCursor,
) -> Option<String> {
    let mut result = String::new();

    for bi in start.block_idx..=end.block_idx.min(msg.blocks.len().saturating_sub(1)) {
        let block = msg.blocks.get(bi)?;
        let text = block.raw_text();

        let byte_start = if bi == start.block_idx {
            floor_char_boundary(text, start.byte_offset)
        } else {
            0
        };
        let byte_end = if bi == end.block_idx {
            inclusive_end(text, end.byte_offset)
        } else {
            text.len()
        };

        if byte_start < byte_end {
            result.push_str(&text[byte_start..byte_end]);
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
}

impl SelectionDrag {
    pub fn start(&mut self, cursor: SemanticCursor) {
        self.active = true;
        self.anchor = Some(cursor);
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
        // Keep anchor so the selection remains; it will be cleared externally.
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.anchor = None;
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
            get_selected_text(&sel, &messages),
            Some("Hello world".to_string())
        );
    }

    #[test]
    fn expanded_tool_step_copy_uses_semantic_detail() {
        let mut message = ChatMessage::tool_step("call_1", "read_file", r#"{"path":"README.md"}"#);
        message.finish_tool_step("call_1", "file contents", 42);
        message.set_tool_step_expanded(true);
        let code_index = message
            .blocks
            .iter()
            .position(|block| matches!(block, Block::Code { .. }))
            .unwrap();

        let copied = get_selected_text(
            &SelectionState::Block {
                message_idx: 0,
                block_idx: code_index,
            },
            &[message],
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
            get_selected_text(&sel, &messages),
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
            get_selected_text(&sel, &messages),
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
            get_selected_text(&sel, &messages),
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
            get_selected_text(&sel, &messages),
            Some("短文本".to_string())
        );
    }
}
