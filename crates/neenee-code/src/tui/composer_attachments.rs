//! Attachment "chips" staged behind short placeholders inside the live input
//! box, mirroring the paste UX in codex / claude-code / opencode:
//!
//! - Pasting an image inserts `[Image #N]` into the input text and stages the
//!   base64 payload in `pending_images`. The chip is the user's visible
//!   affordance that an attachment will ship with the next message.
//! - Pasting a large block of text inserts `[Pasted text #N +M lines]` and
//!   stages the full text in `pending_text_pastes`, so the input box stays
//!   compact instead of being flooded by thousands of lines.
//! - A single `Backspace` against either chip removes the whole chip (plus
//!   any one trailing space inserted alongside it) in one keystroke, and the
//!   matching staged entry is dropped on the next reconcile pass.
//!
//! The chip text lives inline in `App::input` (codex / claude-code style
//! rather than codex's separate element list) so that ordinary cursor
//! movement, selection, and copy keep working without a custom text buffer.
//! A small scan pass (`reconcile`) runs after each input mutation to prune
//! orphaned staged entries and relabel surviving chips so their `#N` always
//! matches their 1-based position in the staged vectors.

use neenee_core::ImagePart;

/// A pasted-text block becomes a chip when its size crosses this threshold,
/// mirroring codex's `LARGE_PASTE_CHAR_THRESHOLD` (1000) and claude-code's
/// `PASTE_THRESHOLD` (800). The chosen 1024 sits between them and matches
/// the user expectation that ordinary short snippets — single-line commands,
/// URLs, a paragraph of prose — keep flowing through the cursor as ordinary
/// text, while a truly large paste (a source file, a stack trace, a long
/// log dump) collapses into a one-line placeholder so the input box stays
/// compact.
pub(crate) const LARGE_PASTE_CHAR_THRESHOLD: usize = 1024;

/// Classification of a parsed chip. Mirrors the staged-attachment vectors on
/// [`crate::tui::App`]: image chips reference `pending_images`, paste chips
/// reference `pending_text_pastes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Image,
    Paste,
}

/// A chip scanned out of the live input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipMatch {
    pub kind: ChipKind,
    /// The `N` in `#N`, 1-based.
    pub number: usize,
    /// For paste chips, the `M` in `+M lines` if present.
    pub line_count: Option<usize>,
    /// Inclusive start byte offset of the chip's opening `[`.
    pub start_byte: usize,
    /// Exclusive end byte offset just past the chip's closing `]`.
    pub end_byte: usize,
}

/// Build the `[Image #N]` label staged behind a pasted image attachment.
pub fn image_chip(number_one_based: usize) -> String {
    format!("[Image #{number_one_based}]")
}

/// Build the `[Pasted text #N +M lines]` label staged behind a large pasted
/// text block. `line_count` is the number of logical lines (`\n`-separated)
/// in the original paste, shown so the user can tell at a glance how big the
/// hidden payload is.
pub fn paste_chip(number_one_based: usize, line_count: usize) -> String {
    format!("[Pasted text #{number_one_based} +{line_count} lines]")
}

/// Decide whether a pasted text block should be staged behind a chip rather
/// than inlined verbatim. Only size matters: a single threshold of
/// [`LARGE_PASTE_CHAR_THRESHOLD`] chars, so ordinary short pastes (a
/// command, a URL, a paragraph of prose, even a multi-line snippet that
/// stays under the limit) keep flowing through the cursor like an ordinary
/// editor paste. Beyond the threshold the paste collapses into a
/// `[Pasted text #N +M lines]` chip so the input box does not balloon to
/// dozens of wrapped rows.
pub fn should_chip_paste(text: &str) -> bool {
    text.chars().count() > LARGE_PASTE_CHAR_THRESHOLD
}

/// Count the logical lines in a pasted text block for the chip's `+M lines`
/// badge. Matches `str::lines` semantics for empty input and trailing
/// newlines: `"a\nb"` → 2, `"a\nb\n"` → 2, `""` → 0.
pub fn paste_line_count(text: &str) -> usize {
    text.lines().count()
}

/// Parse the inside of a `[...]` chip (without the surrounding brackets).
/// Returns `(kind, number, line_count_opt)` on a syntactic match.
fn parse_chip_body(body: &str) -> Option<(ChipKind, usize, Option<usize>)> {
    if let Some(rest) = body.strip_prefix("Image #") {
        let n = rest.parse::<usize>().ok()?;
        if n == 0 {
            return None;
        }
        return Some((ChipKind::Image, n, None));
    }
    if let Some(rest) = body.strip_prefix("Pasted text #") {
        // Optional ` +M lines` suffix. The number itself is everything up to
        // the first space or end-of-string.
        let (num_part, tail) = match rest.find(' ') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        let n = num_part.parse::<usize>().ok()?;
        if n == 0 {
            return None;
        }
        let line_count = if tail.is_empty() {
            None
        } else if let Some(rest2) = tail.strip_prefix(" +") {
            let digits = rest2.trim_end_matches(" lines");
            digits.parse::<usize>().ok()
        } else {
            None
        };
        return Some((ChipKind::Paste, n, line_count));
    }
    None
}

/// Scan `input` for every well-formed chip in byte order. Used by the
/// reconcile and submit-time expansion passes.
pub fn iter_chips(input: &str) -> Vec<ChipMatch> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Find the matching `]` on the same byte slice. Chips never
            // contain nested brackets, so the first `]` after `[` closes
            // the candidate.
            if let Some(rel) = input[i..].find(']') {
                let end = i + rel + 1;
                let body = &input[i + 1..i + rel];
                if let Some((kind, number, line_count)) = parse_chip_body(body) {
                    out.push(ChipMatch {
                        kind,
                        number,
                        line_count,
                        start_byte: i,
                        end_byte: end,
                    });
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Find a well-formed chip that ends exactly at `byte_cursor` (i.e. its
/// closing `]` is the byte just before the cursor). Used by the Backspace
/// handler to detect an atomic-chip delete target.
pub fn chip_range_ending_at(input: &str, byte_cursor: usize) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    if byte_cursor == 0 || byte_cursor > bytes.len() {
        return None;
    }
    if bytes[byte_cursor - 1] != b']' {
        return None;
    }
    // Scan backward for the nearest `[`.
    let mut start = byte_cursor - 1;
    while start > 0 && bytes[start] != b'[' {
        start -= 1;
    }
    if bytes[start] != b'[' {
        return None;
    }
    let body = input.get(start + 1..byte_cursor - 1)?;
    if parse_chip_body(body).is_some() {
        Some((start, byte_cursor))
    } else {
        None
    }
}

/// Detect a chip targeted by a single `Backspace` at `byte_cursor`. Two
/// shapes are recognized so the keystroke fully undoes a paste:
///
/// 1. `<chip>` immediately before the cursor — chip + cursor (no trailing
///    space, e.g. the user moved the cursor left to land right after `]`).
/// 2. `<chip> ` immediately before the cursor — chip + one trailing space +
///    cursor (the common case, since paste inserts `chip + " "`).
///
/// Returns the `(start_byte, end_byte)` byte range to delete.
pub fn chip_range_for_backspace(input: &str, byte_cursor: usize) -> Option<(usize, usize)> {
    // Case 2: chip + trailing space + cursor.
    if byte_cursor >= 1 && input.as_bytes().get(byte_cursor - 1) == Some(&b' ') {
        // `byte_cursor - 1` lands on the space (ASCII, 1 byte). Check for a
        // chip ending just before the space.
        if let Some((s, _)) = chip_range_ending_at(input, byte_cursor - 1) {
            return Some((s, byte_cursor));
        }
    }
    // Case 1: chip + cursor.
    chip_range_ending_at(input, byte_cursor)
}

/// Reconcile the staged attachment vectors against the chips that survive in
/// `input`. Returns the new input text with all chips relabeled so their
/// `#N` matches their new 1-based position in the truncated vectors.
///
/// Algorithm:
/// 1. Walk chips in input order. Each surviving image chip pulls its payload
///    from `pending_images[old_n - 1]`; each paste chip pulls from
///    `pending_text_pastes[old_n - 1]`. If the lookup is out of bounds or
///    the slot has already been consumed (the same `#N` appeared twice),
///    the chip is treated as orphan and left in place as plain text.
/// 2. Truncate the staged vectors to the surviving counts.
/// 3. Rewrite the chip text so `#N` matches the new index.
///
/// Trailing stale entries (e.g. chip #3 was deleted but `pending_images`
/// still holds three items) are dropped by the truncate.
pub fn reconcile(
    input: &str,
    pending_images: &mut Vec<ImagePart>,
    pending_text_pastes: &mut Vec<String>,
) -> String {
    let chips = iter_chips(input);
    if chips.is_empty() {
        let mut changed = false;
        if !pending_images.is_empty() {
            pending_images.clear();
            changed = true;
        }
        if !pending_text_pastes.is_empty() {
            pending_text_pastes.clear();
            changed = true;
        }
        // No chips in input; nothing to rewrite regardless.
        let _ = changed;
        return input.to_string();
    }

    // First pass: collect surviving payloads in chip order, tracking which
    // original indices have already been consumed so a repeated chip (e.g.
    // the user copy-pasted `[Image #1]` twice) does not double-spend the
    // same backing slot.
    let mut new_images: Vec<ImagePart> = Vec::new();
    let mut new_pastes: Vec<String> = Vec::new();
    let mut consumed_image_slots: Vec<bool> = vec![false; pending_images.len()];
    let mut consumed_paste_slots: Vec<bool> = vec![false; pending_text_pastes.len()];
    let mut chip_payload: Vec<Option<(ChipKind, usize)>> = Vec::with_capacity(chips.len());
    for chip in &chips {
        match chip.kind {
            ChipKind::Image => {
                let slot = chip.number.saturating_sub(1);
                let taken = consumed_image_slots.get(slot).copied().unwrap_or(true);
                if !taken {
                    if let Some(image) = pending_images.get(slot) {
                        new_images.push(image.clone());
                        consumed_image_slots[slot] = true;
                        chip_payload.push(Some((ChipKind::Image, new_images.len())));
                        continue;
                    }
                }
                chip_payload.push(None);
            }
            ChipKind::Paste => {
                let slot = chip.number.saturating_sub(1);
                let taken = consumed_paste_slots.get(slot).copied().unwrap_or(true);
                if !taken {
                    if let Some(text) = pending_text_pastes.get(slot) {
                        new_pastes.push(text.clone());
                        consumed_paste_slots[slot] = true;
                        chip_payload.push(Some((ChipKind::Paste, new_pastes.len())));
                        continue;
                    }
                }
                chip_payload.push(None);
            }
        }
    }

    // Second pass: rebuild the input, relabeling recognized chips and
    // leaving orphan chips as-is so the user sees what they typed.
    let mut out = String::with_capacity(input.len());
    let mut last_end = 0;
    for (chip, payload) in chips.iter().zip(chip_payload.iter()) {
        out.push_str(&input[last_end..chip.start_byte]);
        match payload {
            Some((ChipKind::Image, n)) => out.push_str(&image_chip(*n)),
            Some((ChipKind::Paste, n)) => {
                out.push_str(&paste_chip(*n, chip.line_count.unwrap_or(0)))
            }
            None => out.push_str(&input[chip.start_byte..chip.end_byte]),
        }
        last_end = chip.end_byte;
    }
    out.push_str(&input[last_end..]);

    *pending_images = new_images;
    *pending_text_pastes = new_pastes;
    out
}

/// Replace every paste chip in `input` with its staged full text, leaving
/// image chips in place (their payload ships via `AgentRequest::Chat::images`
/// alongside the text, so the model needs the chip's positional label to
/// know where the image belongs in the message). Used at submit time so the
/// model receives the real text rather than the chip label.
pub fn expand_paste_chips(input: &str, pending_text_pastes: &[String]) -> String {
    let chips = iter_chips(input);
    if chips.is_empty() {
        return input.to_string();
    }
    let mut out = String::with_capacity(
        input.len() + pending_text_pastes.iter().map(|s| s.len()).sum::<usize>(),
    );
    let mut last_end = 0;
    let mut paste_index = 0;
    for chip in &chips {
        if chip.kind != ChipKind::Paste {
            continue;
        }
        out.push_str(&input[last_end..chip.start_byte]);
        // The chip's `#N` matches `pending_text_pastes[N-1]` because
        // reconcile has already run by submit time.
        let slot = chip.number.saturating_sub(1);
        if let Some(text) = pending_text_pastes.get(slot) {
            out.push_str(text);
        } else {
            out.push_str(&input[chip.start_byte..chip.end_byte]);
        }
        paste_index += 1;
        last_end = chip.end_byte;
    }
    out.push_str(&input[last_end..]);
    let _ = paste_index;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_chip_paste_thresholds() {
        // Short and even multi-line: inline.
        assert!(!should_chip_paste("hello"));
        assert!(!should_chip_paste(&"a".repeat(1024)));
        assert!(!should_chip_paste("a\nb\nc\nd"));
        // At least one char past the threshold: chip.
        assert!(should_chip_paste(&"a".repeat(1025)));
        // A many-line paste under the char threshold still inlines.
        let many_short_lines = "x\n".repeat(100);
        assert!(!should_chip_paste(many_short_lines.trim_end()));
    }

    #[test]
    fn paste_line_count_matches_str_lines() {
        assert_eq!(paste_line_count(""), 0);
        assert_eq!(paste_line_count("one"), 1);
        assert_eq!(paste_line_count("a\nb"), 2);
        assert_eq!(paste_line_count("a\nb\n"), 2);
    }

    #[test]
    fn image_chip_format() {
        assert_eq!(image_chip(1), "[Image #1]");
        assert_eq!(image_chip(7), "[Image #7]");
    }

    #[test]
    fn paste_chip_format() {
        assert_eq!(paste_chip(1, 5), "[Pasted text #1 +5 lines]");
        assert_eq!(paste_chip(3, 0), "[Pasted text #3 +0 lines]");
    }

    #[test]
    fn iter_chips_finds_image_and_paste() {
        let input = format!("pre {} mid {} end", image_chip(1), paste_chip(2, 10));
        let chips = iter_chips(&input);
        assert_eq!(chips.len(), 2);
        assert_eq!(chips[0].kind, ChipKind::Image);
        assert_eq!(chips[0].number, 1);
        assert_eq!(chips[0].line_count, None);
        assert_eq!(chips[1].kind, ChipKind::Paste);
        assert_eq!(chips[1].number, 2);
        assert_eq!(chips[1].line_count, Some(10));
        // Byte ranges point at the actual chip substrings.
        assert_eq!(&input[chips[0].start_byte..chips[0].end_byte], "[Image #1]");
        assert_eq!(
            &input[chips[1].start_byte..chips[1].end_byte],
            "[Pasted text #2 +10 lines]"
        );
    }

    #[test]
    fn iter_chips_ignores_unrelated_brackets() {
        let chips = iter_chips("hello [not a chip] world [1]");
        assert!(chips.is_empty());
    }

    #[test]
    fn chip_range_ending_at_detects_trailing_chip() {
        let input = format!("hello {}", image_chip(1));
        let cursor = input.len();
        assert_eq!(chip_range_ending_at(&input, cursor), Some((6, cursor)));
        // Cursor one byte inside the chip: no match.
        assert_eq!(chip_range_ending_at(&input, cursor - 1), None);
        // Cursor at the space after "hello": no match.
        assert_eq!(chip_range_ending_at(&input, 5), None);
    }

    #[test]
    fn chip_range_for_backspace_handles_trailing_space() {
        let input = format!("hello {} ", image_chip(1));
        let cursor = input.len();
        // Cursor sits after the space; backspace should remove both the
        // space and the chip in one keystroke.
        let chip_start = "hello ".len();
        assert_eq!(
            chip_range_for_backspace(&input, cursor),
            Some((chip_start, cursor))
        );
    }

    #[test]
    fn chip_range_for_backspace_handles_no_trailing_space() {
        let input = format!("hello{}", image_chip(1));
        let cursor = input.len();
        let chip_start = "hello".len();
        assert_eq!(
            chip_range_for_backspace(&input, cursor),
            Some((chip_start, cursor))
        );
    }

    #[test]
    fn reconcile_drops_orphaned_pending_entries() {
        // Input has one image chip but pending_images holds two. Reconcile
        // must truncate the second entry and relabel the surviving chip.
        let mut images = vec![
            ImagePart {
                mime: "image/png".to_string(),
                data: "a".to_string(),
            },
            ImagePart {
                mime: "image/png".to_string(),
                data: "b".to_string(),
            },
        ];
        let mut pastes = Vec::<String>::new();
        let input = format!("look at {}", image_chip(1));
        let out = reconcile(&input, &mut images, &mut pastes);
        assert_eq!(out, input);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, "a");
    }

    #[test]
    fn reconcile_relabels_chips_after_a_middle_deletion() {
        // User pasted three images, then deleted the chip for #2 by hand
        // (e.g. via selection delete). The remaining two chips must be
        // relabeled to #1 and #2 so their numbers stay contiguous with
        // the surviving pending_images entries.
        let mut images = vec![
            ImagePart {
                mime: "image/png".to_string(),
                data: "a".to_string(),
            },
            ImagePart {
                mime: "image/png".to_string(),
                data: "b".to_string(),
            },
            ImagePart {
                mime: "image/png".to_string(),
                data: "c".to_string(),
            },
        ];
        let mut pastes = Vec::<String>::new();
        // Original: "x [Image #1] y [Image #2] z [Image #3]" — but #2 was
        // removed, leaving #1 and #3 out of order.
        let input = format!("x {} y {} z", image_chip(1), image_chip(3));
        let out = reconcile(&input, &mut images, &mut pastes);
        assert_eq!(out, format!("x {} y {} z", image_chip(1), image_chip(2)));
        assert_eq!(images.len(), 2);
        // The first surviving chip pulls pending_images[0] = "a", the
        // second pulls pending_images[2] = "c".
        assert_eq!(images[0].data, "a");
        assert_eq!(images[1].data, "c");
    }

    #[test]
    fn reconcile_preserves_paste_line_counts() {
        let mut images = Vec::<ImagePart>::new();
        let mut pastes = vec!["first\npaste".to_string(), "second\npaste".to_string()];
        let input = format!("{} {}", paste_chip(1, 2), paste_chip(2, 2));
        let out = reconcile(&input, &mut images, &mut pastes);
        assert_eq!(out, input);
        assert_eq!(pastes.len(), 2);
        let _ = out;
    }

    #[test]
    fn reconcile_clears_pending_lists_when_input_has_no_chips() {
        let mut images = vec![ImagePart {
            mime: "image/png".to_string(),
            data: "a".to_string(),
        }];
        let mut pastes = vec!["stale".to_string()];
        let out = reconcile("just plain text", &mut images, &mut pastes);
        assert_eq!(out, "just plain text");
        assert!(images.is_empty());
        assert!(pastes.is_empty());
    }

    #[test]
    fn expand_paste_chips_inlines_text_in_order() {
        let pastes = vec!["AAA".to_string(), "BBB".to_string()];
        let input = format!("pre {} mid {} post", paste_chip(1, 1), paste_chip(2, 1));
        let out = expand_paste_chips(&input, &pastes);
        assert_eq!(out, "pre AAA mid BBB post");
    }

    #[test]
    fn expand_paste_chips_keeps_image_chips() {
        // Image chips are positional labels for the model, not paste
        // payloads — they must survive expansion unchanged.
        let pastes = vec!["AAA".to_string()];
        let input = format!("{} {} {}", image_chip(1), paste_chip(1, 1), image_chip(2));
        let out = expand_paste_chips(&input, &pastes);
        assert_eq!(out, format!("{} AAA {}", image_chip(1), image_chip(2)));
    }

    #[test]
    fn expand_paste_chips_passthrough_when_no_chips() {
        let pastes: Vec<String> = Vec::new();
        let out = expand_paste_chips("plain text only", &pastes);
        assert_eq!(out, "plain text only");
    }
}
