//! Layout engine: maps semantic blocks to screen coordinates.
//!
//! During rendering we record where each block lands on the terminal grid.
//! This allows mouse events to be resolved back to semantic positions.

use neenee_tui::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const TOOL_STEP_BLOCK_IDX: usize = usize::MAX;
pub const THINKING_BLOCK_IDX: usize = usize::MAX - 1;

/// Identifies a specific position inside the document model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticCursor {
    /// Index into the message list.
    pub message_idx: usize,
    /// Index into the message's block list.
    pub block_idx: usize,
    /// Byte offset inside the block's raw text. Hit-testing may place this
    /// inside a grapheme cluster; selection/copy consumers snap it to grapheme
    /// boundaries before slicing.
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
    /// Byte ranges within `text` that are rendered as zero-width (visually
    /// elided) — e.g. the `**` bold marker delimiters. [`Self::text`] still
    /// holds the original bytes (so copy, which resolves against the block's
    /// raw `content`, yields the exact `**bold**` source), but these ranges
    /// occupy no display columns, so [`LayoutMap::cursor_at`] must skip them
    /// when mapping a screen column back to a byte offset. Empty for blocks
    /// with no elided markup.
    pub hidden_ranges: Vec<(usize, usize)>,
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
    /// The visible transcript content rect for the frame: the horizontal band
    /// (inside the `TRANSCRIPT_H_INSET` gutters) spanning only the rows where
    /// transcript content was actually drawn. A click that doesn't resolve to
    /// any region but lands inside this rect still switches keyboard focus to
    /// [`crate::tui::input::FocusZone::Browse`], so gap rows between messages
    /// behave like the content they separate rather than dead zones. The outer
    /// gutters are excluded on purpose: clicks there are not transcript clicks.
    transcript_content_rect: Option<Rect>,
}

/// A clickable region belonging to one logical table cell.
///
/// One rendered line segment belonging to a logical table cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableCellSegment {
    /// Absolute byte range of the padded cell content within the rendered table
    /// grid. This is the range selection rendering understands.
    pub rendered_range: (usize, usize),
    /// Absolute byte range of the actual cell text within the rendered table
    /// grid, excluding alignment padding. Drag endpoints clamp here so padding
    /// clicks resolve to the nearest text boundary.
    pub content_range: (usize, usize),
    /// Byte range in the original, unwrapped cell text represented by this
    /// rendered line segment.
    pub source_range: (usize, usize),
}

/// `cell_text` is the *original* cell text (from `headers` / `rows`, before
/// padding/wrapping). `segment` maps this visible table line back to that
/// source text.
#[derive(Debug, Clone)]
pub struct TableCellHit {
    pub message_idx: usize,
    pub block_idx: usize,
    pub cell_idx: usize,
    pub rect: Rect,
    /// Original cell text, copied from the `Block::Table` headers/rows.
    pub cell_text: String,
    pub segment: TableCellSegment,
}

impl LayoutMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a portion of a block occupies a screen rectangle.
    pub fn push(&mut self, region: BlockRegion) {
        self.regions.push(region);
    }

    /// Record the visible transcript content rect for this frame, drawn inside
    /// the horizontal gutters. Called once at the end of `draw_transcript`.
    pub fn set_transcript_content_rect(&mut self, rect: Rect) {
        self.transcript_content_rect = Some(rect);
    }

    /// The visible transcript content rect, if any content was drawn this frame.
    /// Clicks inside this rect that don't resolve to a specific region still
    /// switch keyboard focus to Browse (see the `SelectionStart` handler).
    pub fn transcript_content_rect(&self) -> Option<Rect> {
        self.transcript_content_rect
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
    pub fn table_cell_at(&self, x: u16, y: u16) -> Option<&TableCellHit> {
        self.table_cell_hits.iter().find(|h| {
            h.rect.x <= x
                && x < h.rect.x + h.rect.width
                && h.rect.y <= y
                && y < h.rect.y + h.rect.height
        })
    }

    pub fn table_cell_segments(
        &self,
        message_idx: usize,
        block_idx: usize,
        cell_idx: usize,
    ) -> Vec<TableCellSegment> {
        self.table_cell_hits
            .iter()
            .filter(|hit| {
                hit.message_idx == message_idx
                    && hit.block_idx == block_idx
                    && hit.cell_idx == cell_idx
            })
            .map(|hit| hit.segment)
            .collect()
    }

    /// Find the semantic cursor at a given screen coordinate.
    ///
    /// The column is resolved against the region's actual text using Unicode
    /// display width, so multi-byte and wide (CJK) characters map to the
    /// correct byte offset. For non-leading columns of a wide grapheme, the
    /// result intentionally sits inside that grapheme: a collapsed click still
    /// compares equal to itself, while an actual drag can distinguish "moved
    /// across this glyph" without jumping to the next glyph. Consumers that
    /// slice text must snap to grapheme boundaries first.
    pub fn cursor_at(&self, x: u16, y: u16) -> Option<SemanticCursor> {
        let region = self.region_at(x, y)?;

        let col_in_rect = x.saturating_sub(region.rect.x);
        let col = col_in_rect.saturating_sub(region.prefix_cols) as usize;

        // Walk the rendered text, accumulating display width until we reach
        // the clicked column. The cursor lands at the start of the character
        // occupying that column. Bytes that fall inside a `hidden_ranges`
        // entry are visually elided (zero-width, e.g. `**` bold markers), so
        // they advance the byte cursor but contribute no display columns —
        // keeping the screen-column → byte-offset mapping in lockstep with
        // what the user actually sees.
        let mut acc_width = 0usize;
        for (byte_idx, grapheme) in region.text.grapheme_indices(true) {
            if region
                .hidden_ranges
                .iter()
                .any(|&(lo, hi)| byte_idx >= lo && byte_idx < hi)
            {
                continue;
            }
            let w = if grapheme == "\n" {
                0
            } else {
                grapheme.width().max(1)
            };
            if col < acc_width + w {
                let target_byte = if col == acc_width || grapheme.len() <= 1 {
                    byte_idx
                } else {
                    byte_idx + 1
                };
                return Some(SemanticCursor::new(
                    region.message_idx,
                    region.block_idx,
                    region.start_byte + target_byte,
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
            hidden_ranges: Vec::new(),
        }
    }

    #[test]
    fn test_cursor_at_basic() {
        let mut map = LayoutMap::new();
        map.push(region("hello", 0, 0, Rect::new(0, 0, 10, 1)));

        let cursor = map.cursor_at(2, 0).unwrap();
        assert_eq!(cursor.message_idx, 0);
        assert_eq!(cursor.block_idx, 0);
        assert_eq!(cursor.byte_offset, 2);
    }

    #[test]
    fn test_cursor_at_miss() {
        let map = LayoutMap::new();
        assert!(map.cursor_at(0, 0).is_none());
    }

    #[test]
    fn cursor_at_subtracts_prefix_columns() {
        let mut map = LayoutMap::new();
        map.push(region("hello", 0, 3, Rect::new(0, 0, 20, 1)));

        // Column 3 is the first text column.
        assert_eq!(map.cursor_at(3, 0).unwrap().byte_offset, 0);
        assert_eq!(map.cursor_at(5, 0).unwrap().byte_offset, 2);
        // Inside the prefix clamps to the line start.
        assert_eq!(map.cursor_at(1, 0).unwrap().byte_offset, 0);
    }

    #[test]
    fn cursor_at_handles_wide_and_multibyte_chars() {
        // "😀😃a" — 😀/😃 are 4 bytes, 2 columns each.
        let mut map = LayoutMap::new();
        map.push(region("😀😃a", 0, 0, Rect::new(0, 0, 20, 1)));

        // The leading column resolves to the glyph start; the trailing column
        // resolves inside the glyph so inclusive selection can cover exactly
        // this glyph without spilling into the next one.
        assert_eq!(map.cursor_at(0, 0).unwrap().byte_offset, 0);
        assert_eq!(map.cursor_at(1, 0).unwrap().byte_offset, 1);
        // 😃 starts at byte 4 (columns 2-3).
        assert_eq!(map.cursor_at(2, 0).unwrap().byte_offset, 4);
        assert_eq!(map.cursor_at(3, 0).unwrap().byte_offset, 5);
        // 'a' at byte 8, column 4.
        assert_eq!(map.cursor_at(4, 0).unwrap().byte_offset, 8);
        // Past the end clamps to end_byte — always a char boundary.
        assert_eq!(map.cursor_at(15, 0).unwrap().byte_offset, 9);
    }

    #[test]
    fn cursor_at_respects_wrapped_line_offsets() {
        // Second wrapped line of a block starting at byte 10.
        let mut map = LayoutMap::new();
        map.push(region("world", 10, 3, Rect::new(0, 4, 20, 1)));

        assert_eq!(map.cursor_at(4, 4).unwrap().byte_offset, 11);
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
            hidden_ranges: Vec::new(),
        });
        map.push(BlockRegion {
            message_idx: 2,
            block_idx: TOOL_STEP_BLOCK_IDX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: Rect::new(0, 6, 10, 1),
            hidden_ranges: Vec::new(),
        });
        map.push(BlockRegion {
            message_idx: 3,
            block_idx: THINKING_BLOCK_IDX,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: Rect::new(0, 7, 10, 1),
            hidden_ranges: Vec::new(),
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
