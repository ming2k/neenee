//! Text measurement and wrapping: grapheme-cluster width and CJK-aware line
//! breaking.
//!
//! This is the *engine* half of the old `render/text_layout.rs`, stripped of
//! any ratatui or selection vocabulary. It answers two questions the grid and
//! the application both need:
//!
//! - How many columns does a grapheme occupy? ([`grapheme_width`])
//! - How does a string break into lines of at most `N` columns, honoring CJK
//!   kinsoku (line-start/line-end prohibition) rules? ([`wrap`])
//!
//! The kinsoku tables are lifted verbatim from the prior `text_layout.rs` so
//! wrapping behavior stays identical for any content the application routes
//! through the engine.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// The display width of a string, summing each grapheme's width.
pub fn str_len(s: &str) -> usize {
    s.graphemes(true).map(|g| grapheme_width(g) as usize).sum()
}

/// Alias for [`str_len`] (display columns). Named to match callers that think
/// in terms of "string display width".
pub fn str_idth(s: &str) -> usize {
    str_len(s)
}

/// A slightly different width measure that treats the string as a whole via
/// `UnicodeWidthStr`, used where callers previously called `UnicodeWidthStr`.
pub fn str_len_w(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// The column width of a single grapheme cluster.
///
/// A grapheme may be several code points (e.g. `e` + combining acute). Its
/// display width is the width of its *base* character; zero-width combining
/// marks and ZWJ-joined emoji sequences contribute nothing beyond the base.
/// Control characters (including embedded newlines) report width 0 so the
/// application can handle them structurally rather than as visible glyphs.
pub fn grapheme_width(grapheme: &str) -> u8 {
    // The base char is the first code point; combining marks that follow are
    // zero-width. This is the same rule `UnicodeWidthChar` encodes per-char.
    match grapheme.chars().next() {
        Some(ch) => ch.width().unwrap_or(0) as u8,
        None => 0,
    }
}

/// A grapheme broken out with its precomputed width and byte range. Returned
/// by [`graphemes`] so callers that walk a string column-by-column (the
/// wrapper, the grid's string writer) never re-measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece<'a> {
    pub text: &'a str,
    pub width: u8,
}

/// Iterate a string as width-annotated grapheme clusters. Newlines are kept
/// as standalone zero-width pieces so the wrapper can treat them as hard
/// breaks; other control characters are passed through with width 0.
pub fn graphemes(s: &str) -> impl Iterator<Item = Piece<'_>> {
    s.graphemes(true).map(|g| Piece {
        text: g,
        width: if g == "\n" { 0 } else { grapheme_width(g) },
    })
}

/// A wrapped line: the text it contains and its byte range in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Wrap `text` into lines that each fit within `max_width` display columns.
///
/// Hard newlines start a new line. When a grapheme would overflow the column
/// budget, the line breaks before it — but CJK kinsoku rules pull a
/// line-start-prohibited glyph (e.g. `，`) back onto the previous line, and a
/// line-end-prohibited glyph (e.g. `（`) forward onto the next, so punctuation
/// never strands. The tables and the break math are lifted from the prior
/// `text_layout.rs::wrap_text` so behavior is identical.
pub fn wrap(text: &str, max_width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut line_start_byte = 0usize;

    // Iterate by *char* (not grapheme) so byte offsets are exact via
    // `char_indices`, matching the prior implementation. Combining marks have
    // width 0, so they ride along on their base char without growing the
    // column budget; kinsoku is tested per char, which is sufficient because
    // the kinsoku tables are all base characters.
    for (byte_idx, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);

        if ch == '\n' {
            lines.push(Line {
                text: std::mem::take(&mut current),
                start_byte: line_start_byte,
                end_byte: byte_idx,
            });
            line_start_byte = byte_idx + ch.len_utf8();
            current_width = 0;
            continue;
        }

        if current_width + ch_width > max_width && !current.is_empty() {
            // Kinsoku: avoid starting the next line with a closing punct, or
            // ending it with an opening punct. Pull one char across the break
            // to keep the pair intact.
            let move_previous = prohibited_line_start(ch)
                || current.chars().last().is_some_and(prohibited_line_end);
            let to_move = (move_previous && current.chars().count() > 1)
                .then(|| current.pop())
                .flatten();
            if let Some(moved) = to_move {
                let moved_start = byte_idx - moved.len_utf8();
                lines.push(Line {
                    text: std::mem::take(&mut current),
                    start_byte: line_start_byte,
                    end_byte: moved_start,
                });
                current.push(moved);
                current_width = UnicodeWidthChar::width(moved).unwrap_or(0);
                line_start_byte = moved_start;
            } else {
                lines.push(Line {
                    text: std::mem::take(&mut current),
                    start_byte: line_start_byte,
                    end_byte: byte_idx,
                });
                line_start_byte = byte_idx;
                current_width = 0;
            }
        }

        current.push(ch);
        current_width += ch_width;
    }

    if !current.is_empty() || line_start_byte < text.len() {
        lines.push(Line {
            text: current,
            start_byte: line_start_byte,
            end_byte: text.len(),
        });
    }

    lines
}

/// A character that must not start a line (CJK closing punctuation, and the
/// ASCII closers so mixed CJK/Latin text also breaks cleanly).
pub fn prohibited_line_start(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '、'
            | '！'
            | '？'
            | '：'
            | '；'
            | '）'
            | '】'
            | '》'
            | '〉'
            | '」'
            | '』'
            | '〕'
            | '”'
            | '’'
            | ','
            | '.'
            | '!'
            | '?'
            | ':'
            | ';'
            | ')'
            | ']'
            | '}'
    )
}

/// A character that must not end a line (CJK opening punctuation, and the
/// ASCII openers).
pub fn prohibited_line_end(ch: char) -> bool {
    matches!(
        ch,
        '（' | '【' | '《' | '〈' | '「' | '『' | '〔' | '“' | '‘' | '(' | '[' | '{'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_wraps_on_word_overflow() {
        let lines = wrap("hello world", 5);
        // "hello" then " worl" then "d" — matches the prior implementation's
        // greedy break (it does not do word splitting, just column budget).
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text, " worl");
        assert_eq!(lines[2].text, "d");
    }

    #[test]
    fn newline_is_a_hard_break() {
        let lines = wrap("hi\nthere", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "hi");
        assert_eq!(lines[1].text, "there");
        // Byte ranges cover the source exactly, including the newline.
        assert_eq!(lines[0].start_byte, 0);
        assert_eq!(lines[0].end_byte, 2);
        assert_eq!(lines[1].start_byte, 3);
        assert_eq!(lines[1].end_byte, 8);
    }

    #[test]
    fn cjk_avoids_prohibited_line_start() {
        // Comma must not start a line; the preceding char comes with it.
        let lines = wrap("人生需要坚持，才能前进。", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().skip(1).all(|l| {
            l.text
                .chars()
                .next()
                .is_none_or(|ch| !prohibited_line_start(ch))
        }));
        assert!(lines.iter().all(|l| {
            l.text
                .chars()
                .last()
                .is_none_or(|ch| !prohibited_line_end(ch))
        }));
    }

    #[test]
    fn wide_grapheme_has_width_two() {
        assert_eq!(grapheme_width("中"), 2);
        assert_eq!(grapheme_width("a"), 1);
        // Combining mark: base width only.
        assert_eq!(grapheme_width("e\u{0301}"), 1);
    }

    #[test]
    fn empty_string_yields_no_lines() {
        assert!(wrap("", 10).is_empty());
    }
}
