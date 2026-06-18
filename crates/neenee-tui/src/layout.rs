//! Layout engine: maps semantic blocks to screen coordinates.
//!
//! During rendering we record where each block lands on the terminal grid.
//! This allows mouse events to be resolved back to semantic positions.

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

pub const TOOL_STEP_BLOCK_IDX: usize = usize::MAX;
pub const THINKING_BLOCK_IDX: usize = usize::MAX - 1;

/// Identifies a specific position inside the document model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticCursor {
    /// Index into the message list.
    pub message_idx: usize,
    /// Index into the message's block list.
    pub block_idx: usize,
    /// Byte offset inside the block's raw text.
    pub byte_offset: usize,
}

impl SemanticCursor {
    pub fn new(message_idx: usize, block_idx: usize, byte_offset: usize) -> Self {
        Self {
            message_idx,
            block_idx,
            byte_offset,
        }
    }
}

/// User-activatable target recorded from the semantic layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveTarget {
    pub message_idx: usize,
    pub block_idx: usize,
    pub kind: InteractiveTargetKind,
}

/// Category of an activatable target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveTargetKind {
    ToolStep,
    Thinking,
}

impl InteractiveTarget {
    pub fn tool_step(message_idx: usize) -> Self {
        Self {
            message_idx,
            block_idx: TOOL_STEP_BLOCK_IDX,
            kind: InteractiveTargetKind::ToolStep,
        }
    }

    pub fn thinking(message_idx: usize) -> Self {
        Self {
            message_idx,
            block_idx: THINKING_BLOCK_IDX,
            kind: InteractiveTargetKind::Thinking,
        }
    }
}

/// A rectangular region on screen that corresponds to a slice of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRegion {
    pub message_idx: usize,
    pub block_idx: usize,
    /// Byte offset of the first character displayed in this region.
    pub start_byte: usize,
    /// Byte offset of the first character *after* this region.
    pub end_byte: usize,
    /// The exact text slice rendered in this region (no indent/prefix).
    pub text: String,
    /// Display columns occupied by decoration before the text (indent, `│ `).
    pub prefix_cols: u16,
    /// Screen rectangle (inclusive start, exclusive end in x; y is absolute row).
    pub rect: Rect,
}

/// Records the layout of rendered blocks for a single frame.
#[derive(Debug, Clone, Default)]
pub struct LayoutMap {
    regions: Vec<BlockRegion>,
    /// The displayed grid text for each `Block::Table`, keyed by
    /// `(message_idx, block_idx)`. Stored at render time because table
    /// columns are reshaped to fit the viewport, so this grid can differ
    /// from the width-independent `rendered` field stored on the block.
    /// Whole-table copy (middle-click) resolves against this text.
    table_grids: std::collections::HashMap<(usize, usize), String>,
    /// Hit boxes for individual table cells, so a click inside a cell resolves
    /// to that cell (row-major index: `row * ncols + col`, header is row 0)
    /// rather than to the whole grid line.
    table_cell_hits: Vec<TableCellHit>,
}

/// A clickable region belonging to one logical table cell.
#[derive(Debug, Clone)]
pub struct TableCellHit {
    pub message_idx: usize,
    pub block_idx: usize,
    pub cell_idx: usize,
    pub rect: Rect,
}

impl LayoutMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a portion of a block occupies a screen rectangle.
    pub fn push(&mut self, region: BlockRegion) {
        self.regions.push(region);
    }

    /// Clear all regions (call at the start of each frame).
    pub fn clear(&mut self) {
        self.regions.clear();
        self.table_grids.clear();
        self.table_cell_hits.clear();
    }

    /// Record the displayed grid text for a table block.
    pub fn record_table_grid(&mut self, message_idx: usize, block_idx: usize, text: String) {
        self.table_grids.insert((message_idx, block_idx), text);
    }

    /// Look up the displayed grid text previously recorded for a table block.
    pub fn table_grid(&self, message_idx: usize, block_idx: usize) -> Option<&str> {
        self.table_grids
            .get(&(message_idx, block_idx))
            .map(String::as_str)
    }

    /// Record a clickable hit box for one table cell.
    pub fn push_table_cell_hit(&mut self, hit: TableCellHit) {
        self.table_cell_hits.push(hit);
    }

    /// Resolve a screen point to the table cell it lies inside, if any.
    pub fn table_cell_at(&self, x: u16, y: u16) -> Option<(usize, usize, usize)> {
        self.table_cell_hits
            .iter()
            .find(|h| {
                h.rect.x <= x
                    && x < h.rect.x + h.rect.width
                    && h.rect.y <= y
                    && y < h.rect.y + h.rect.height
            })
            .map(|h| (h.message_idx, h.block_idx, h.cell_idx))
    }

    /// Clamp a screen point so it stays inside the hit boxes of the given
    /// table cell. This lets a drag selection roam freely within one cell —
    /// across its wrapped lines and full column width — without ever crossing
    /// a `│` border into a neighbour. Returns `None` if the cell has no
    /// recorded hit boxes for the current frame.
    pub fn clamp_to_table_cell(
        &self,
        cell: (usize, usize, usize),
        x: u16,
        y: u16,
    ) -> Option<(u16, u16)> {
        let hits: Vec<&TableCellHit> = self
            .table_cell_hits
            .iter()
            .filter(|h| (h.message_idx, h.block_idx, h.cell_idx) == cell)
            .collect();
        if hits.is_empty() {
            return None;
        }

        // Prefer the hit box on the same row as `y`; otherwise snap to the
        // nearest cell row so vertical overflow stays within the cell.
        let on_row = hits.iter().find(|h| h.rect.y == y).or_else(|| {
            hits.iter()
                .min_by_key(|h| (h.rect.y as i32 - y as i32).abs())
        })?;
        let row_y = on_row.rect.y;
        let max_x = on_row.rect.x + on_row.rect.width.saturating_sub(1);
        let clamped_x = x.max(on_row.rect.x).min(max_x);
        Some((clamped_x, row_y))
    }

    /// Find the semantic cursor at a given screen coordinate.
    ///
    /// The column is resolved against the region's actual text using Unicode
    /// display width, so multi-byte and wide (CJK) characters map to the
    /// correct byte offset. The result always lies on a char boundary and is
    /// clamped to the region's byte range.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<SemanticCursor> {
        let region = self.region_at(x, y)?;

        let col_in_rect = x.saturating_sub(region.rect.x);
        let col = col_in_rect.saturating_sub(region.prefix_cols) as usize;

        // Walk the rendered text, accumulating display width until we reach
        // the clicked column. The cursor lands at the start of the character
        // occupying that column.
        let mut acc_width = 0usize;
        for (byte_idx, ch) in region.text.char_indices() {
            let w = ch.width().unwrap_or(0).max(1);
            if col < acc_width + w {
                return Some(SemanticCursor::new(
                    region.message_idx,
                    region.block_idx,
                    region.start_byte + byte_idx,
                ));
            }
            acc_width += w;
        }

        // Past the end of the line: cursor sits after the last character.
        Some(SemanticCursor::new(
            region.message_idx,
            region.block_idx,
            region.end_byte.max(region.start_byte),
        ))
    }

    /// Find the region containing a screen point (x, y).
    pub fn region_at(&self, x: u16, y: u16) -> Option<&BlockRegion> {
        self.regions.iter().find(|r| {
            r.rect.x <= x
                && x < r.rect.x + r.rect.width
                && r.rect.y <= y
                && y < r.rect.y + r.rect.height
        })
    }

    /// Returns all regions for a given message.
    pub fn message_regions(&self, message_idx: usize) -> Vec<&BlockRegion> {
        self.regions
            .iter()
            .filter(|r| r.message_idx == message_idx)
            .collect()
    }

    /// Returns all regions for a specific block.
    pub fn block_regions(&self, message_idx: usize, block_idx: usize) -> Vec<&BlockRegion> {
        self.regions
            .iter()
            .filter(|r| r.message_idx == message_idx && r.block_idx == block_idx)
            .collect()
    }

    /// Return visible activatable targets in screen order.
    pub fn interactive_targets(&self) -> Vec<InteractiveTarget> {
        let mut regions: Vec<&BlockRegion> = self
            .regions
            .iter()
            .filter(|region| matches!(region.block_idx, TOOL_STEP_BLOCK_IDX | THINKING_BLOCK_IDX))
            .collect();
        regions.sort_by_key(|region| (region.rect.y, region.rect.x));

        let mut targets = Vec::new();
        for region in regions {
            let target = match region.block_idx {
                TOOL_STEP_BLOCK_IDX => InteractiveTarget::tool_step(region.message_idx),
                THINKING_BLOCK_IDX => InteractiveTarget::thinking(region.message_idx),
                _ => continue,
            };
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }
}

/// Helper to measure how many display columns a string occupies.
pub fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(text: &str, start_byte: usize, prefix_cols: u16, rect: Rect) -> BlockRegion {
        BlockRegion {
            message_idx: 0,
            block_idx: 0,
            start_byte,
            end_byte: start_byte + text.len(),
            text: text.to_string(),
            prefix_cols,
            rect,
        }
    }

    #[test]
    fn test_hit_test_basic() {
        let mut map = LayoutMap::new();
        map.push(region("hello", 0, 0, Rect::new(0, 0, 10, 1)));

        let cursor = map.hit_test(2, 0).unwrap();
        assert_eq!(cursor.message_idx, 0);
        assert_eq!(cursor.block_idx, 0);
        assert_eq!(cursor.byte_offset, 2);
    }

    #[test]
    fn test_hit_test_miss() {
        let map = LayoutMap::new();
        assert!(map.hit_test(0, 0).is_none());
    }

    #[test]
    fn hit_test_subtracts_prefix_columns() {
        let mut map = LayoutMap::new();
        map.push(region("hello", 0, 3, Rect::new(0, 0, 20, 1)));

        // Column 3 is the first text column.
        assert_eq!(map.hit_test(3, 0).unwrap().byte_offset, 0);
        assert_eq!(map.hit_test(5, 0).unwrap().byte_offset, 2);
        // Inside the prefix clamps to the line start.
        assert_eq!(map.hit_test(1, 0).unwrap().byte_offset, 0);
    }

    #[test]
    fn hit_test_handles_wide_and_multibyte_chars() {
        // "你好a" — 你/好 are 3 bytes, 2 columns each.
        let mut map = LayoutMap::new();
        map.push(region("你好a", 0, 0, Rect::new(0, 0, 20, 1)));

        // Both columns of 你 resolve to byte 0.
        assert_eq!(map.hit_test(0, 0).unwrap().byte_offset, 0);
        assert_eq!(map.hit_test(1, 0).unwrap().byte_offset, 0);
        // 好 starts at byte 3 (columns 2-3).
        assert_eq!(map.hit_test(2, 0).unwrap().byte_offset, 3);
        assert_eq!(map.hit_test(3, 0).unwrap().byte_offset, 3);
        // 'a' at byte 6, column 4.
        assert_eq!(map.hit_test(4, 0).unwrap().byte_offset, 6);
        // Past the end clamps to end_byte — always a char boundary.
        assert_eq!(map.hit_test(15, 0).unwrap().byte_offset, 7);
    }

    #[test]
    fn hit_test_respects_wrapped_line_offsets() {
        // Second wrapped line of a block starting at byte 10.
        let mut map = LayoutMap::new();
        map.push(region("world", 10, 3, Rect::new(0, 4, 20, 1)));

        assert_eq!(map.hit_test(4, 4).unwrap().byte_offset, 11);
    }

    #[test]
    fn interactive_targets_are_visible_ordered_and_deduplicated() {
        let mut map = LayoutMap::new();
        map.push(BlockRegion {
            message_idx: 2,
            block_idx: TOOL_STEP_BLOCK_IDX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: Rect::new(0, 5, 10, 1),
        });
        map.push(BlockRegion {
            message_idx: 2,
            block_idx: TOOL_STEP_BLOCK_IDX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: Rect::new(0, 6, 10, 1),
        });
        map.push(BlockRegion {
            message_idx: 3,
            block_idx: THINKING_BLOCK_IDX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: Rect::new(0, 7, 10, 1),
        });

        assert_eq!(
            map.interactive_targets(),
            vec![
                InteractiveTarget::tool_step(2),
                InteractiveTarget::thinking(3)
            ]
        );
    }
}
