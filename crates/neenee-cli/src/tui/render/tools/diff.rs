//! Line-level diff used to visualize `edit_file` / `write_file` changes.
//!
//! Backed by `similar`'s Myers line diff (so multi-hunk, interleaved edits
//! render correctly rather than collapsing to all-removed-then-all-added).
//! Each output line carries its 1-based old/new line number, and adjacent
//! delete/insert pairs are further split into word-level fragments so the
//! exact edited span is highlighted within a changed line.

use similar::{ChangeTag, TextDiff};

/// `(added, removed)` line counts for the change from `old` to `new`. Used for
/// the `+N -M` summary suffix in the step header. Computed from the real diff
/// so the count always matches what the body renders.
pub fn line_diff_counts(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0usize;
    let mut removed = 0usize;
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
    }
    (added, removed)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffOp {
    /// Unchanged line shown for context.
    Context,
    /// Line present only in the new text.
    Add,
    /// Line present only in the old text.
    Remove,
}

/// One intra-line fragment: `text` plus whether it is part of the edited span
/// (highlighted by the renderer). Lines that were not word-diffed carry a
/// single `changed = false` fragment equal to the whole line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFrag {
    pub text: String,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub op: DiffOp,
    /// 1-based line number in the old text (set for `Remove` and `Context`).
    pub old_no: Option<usize>,
    /// 1-based line number in the new text (set for `Add` and `Context`).
    pub new_no: Option<usize>,
    /// Word-level fragments; the concatenation equals the line text.
    pub frags: Vec<DiffFrag>,
}

impl DiffLine {
    fn context(text: &str, old_no: usize, new_no: usize) -> Self {
        DiffLine {
            op: DiffOp::Context,
            old_no: Some(old_no),
            new_no: Some(new_no),
            frags: vec![DiffFrag {
                text: text.to_string(),
                changed: false,
            }],
        }
    }

    fn plain(op: DiffOp, text: &str, no: usize) -> Self {
        let (old_no, new_no) = match op {
            DiffOp::Remove => (Some(no), None),
            _ => (None, Some(no)),
        };
        DiffLine {
            op,
            old_no,
            new_no,
            frags: vec![DiffFrag {
                text: text.to_string(),
                changed: false,
            }],
        }
    }

    /// The full line text (all fragments concatenated).
    pub fn text(&self) -> String {
        self.frags.iter().map(|f| f.text.as_str()).collect()
    }
}

/// Word-diff a removed/added line pair, returning the fragments for each side
/// with the differing spans marked `changed`. Uses `similar`'s Unicode word
/// segmentation so identifiers and operators stay intact.
fn word_diff_pair<'a>(old: &'a str, new: &'a str) -> (Vec<DiffFrag>, Vec<DiffFrag>) {
    let diff = TextDiff::from_words(old, new);
    let mut old_frags = Vec::new();
    let mut new_frags = Vec::new();
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let text = change.value().to_string();
            match change.tag() {
                ChangeTag::Equal => {
                    old_frags.push(DiffFrag {
                        text: text.clone(),
                        changed: false,
                    });
                    new_frags.push(DiffFrag {
                        text,
                        changed: false,
                    });
                }
                ChangeTag::Delete => old_frags.push(DiffFrag {
                    text,
                    changed: true,
                }),
                ChangeTag::Insert => new_frags.push(DiffFrag {
                    text,
                    changed: true,
                }),
            }
        }
    }
    (old_frags, new_frags)
}

/// Build the renderable diff with line numbers and intra-line word highlight.
/// Adjacent delete/insert runs are paired up so a one-token edit highlights just
/// that token instead of repainting whole lines.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(old, new);

    // Buffer consecutive deletes/inserts so they can be paired into word-diffs.
    let mut pending_del: Vec<(usize, &str)> = Vec::new();
    let mut pending_ins: Vec<(usize, &str)> = Vec::new();
    let mut out: Vec<DiffLine> = Vec::new();

    let flush =
        |del: &mut Vec<(usize, &str)>, ins: &mut Vec<(usize, &str)>, out: &mut Vec<DiffLine>| {
            let pair = del.len().min(ins.len());
            for i in 0..pair {
                let (old_no, old_text) = del[i];
                let (new_no, new_text) = ins[i];
                let (old_frags, new_frags) = word_diff_pair(old_text, new_text);
                out.push(DiffLine {
                    op: DiffOp::Remove,
                    old_no: Some(old_no + 1),
                    new_no: None,
                    frags: old_frags,
                });
                out.push(DiffLine {
                    op: DiffOp::Add,
                    old_no: None,
                    new_no: Some(new_no + 1),
                    frags: new_frags,
                });
            }
            for &(old_no, old_text) in del.iter().skip(pair) {
                out.push(DiffLine::plain(DiffOp::Remove, old_text, old_no + 1));
            }
            for &(new_no, new_text) in ins.iter().skip(pair) {
                out.push(DiffLine::plain(DiffOp::Add, new_text, new_no + 1));
            }
            del.clear();
            ins.clear();
        };

    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Equal => {
                    flush(&mut pending_del, &mut pending_ins, &mut out);
                    let text = change.value();
                    let old_no = change.old_index().map(|i| i + 1).unwrap_or(0);
                    let new_no = change.new_index().map(|i| i + 1).unwrap_or(0);
                    out.push(DiffLine::context(text, old_no, new_no));
                }
                ChangeTag::Delete => {
                    if !pending_ins.is_empty() {
                        // A new change block started after an insert; flush first.
                        flush(&mut pending_del, &mut pending_ins, &mut out);
                    }
                    if let Some(i) = change.old_index() {
                        pending_del.push((i, change.value()));
                    }
                }
                ChangeTag::Insert => {
                    if let Some(i) = change.new_index() {
                        pending_ins.push((i, change.value()));
                    }
                }
            }
        }
    }
    flush(&mut pending_del, &mut pending_ins, &mut out);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_match_real_diff() {
        assert_eq!(line_diff_counts("a\nb\nc\nd", "a\nB\nc\nd"), (1, 1));
        assert_eq!(line_diff_counts("", "x\ny\nz"), (3, 0));
        assert_eq!(line_diff_counts("x\ny", ""), (0, 2));
    }

    #[test]
    fn paired_change_highlights_only_the_word() {
        let diff = line_diff("let x = 1;", "let x = 2;");
        // Context-free single-line edit: one Remove, one Add.
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].op, DiffOp::Remove);
        assert_eq!(diff[1].op, DiffOp::Add);
        // The differing token is marked changed; the shared prefix is not.
        // (Token boundaries depend on `similar`'s word segmentation, so we
        // assert membership rather than an exact token.)
        let del_changed: String = diff[0]
            .frags
            .iter()
            .filter(|f| f.changed)
            .map(|f| f.text.as_str())
            .collect();
        let del_unchanged: String = diff[0]
            .frags
            .iter()
            .filter(|f| !f.changed)
            .map(|f| f.text.as_str())
            .collect();
        let add_changed: String = diff[1]
            .frags
            .iter()
            .filter(|f| f.changed)
            .map(|f| f.text.as_str())
            .collect();
        assert!(del_changed.contains('1'), "del changed: {del_changed:?}");
        assert!(!del_changed.contains("let"));
        assert!(del_unchanged.contains("let"));
        assert!(add_changed.contains('2'), "add changed: {add_changed:?}");
        assert!(!add_changed.contains("let"));
    }

    #[test]
    fn line_numbers_are_set_and_one_based() {
        let diff = line_diff("a\nb\nc", "a\nB\nc");
        // a(ctx old1/new1), b(del old2), B(add new2), c(ctx old3/new3)
        assert_eq!(diff[0].op, DiffOp::Context);
        assert_eq!(diff[0].old_no, Some(1));
        assert_eq!(diff[0].new_no, Some(1));
        assert_eq!(diff[1].op, DiffOp::Remove);
        assert_eq!(diff[1].old_no, Some(2));
        assert_eq!(diff[2].op, DiffOp::Add);
        assert_eq!(diff[2].new_no, Some(2));
        assert_eq!(diff[3].op, DiffOp::Context);
        assert_eq!(diff[3].old_no, Some(3));
    }

    #[test]
    fn interleaved_edits_do_not_collapse_to_all_remove_then_all_add() {
        let old = "a\nX\nb\nY\nc";
        let new = "a\nx\nb\ny\nc";
        let diff = line_diff(old, new);
        let ops: Vec<_> = diff.iter().map(|d| d.op).collect();
        // Should interleave: Ctx, Remove, Add, Ctx, Remove, Add, Ctx.
        assert_eq!(
            ops,
            vec![
                DiffOp::Context,
                DiffOp::Remove,
                DiffOp::Add,
                DiffOp::Context,
                DiffOp::Remove,
                DiffOp::Add,
                DiffOp::Context,
            ]
        );
    }
}
