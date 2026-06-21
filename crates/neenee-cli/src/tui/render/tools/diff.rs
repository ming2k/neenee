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
    /// Collapsed run of unchanged lines (elided context between changes).
    Ellipsis,
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

    /// A collapsed-context marker line: `⋯` in muted style, no line number.
    fn ellipsis() -> Self {
        DiffLine {
            op: DiffOp::Ellipsis,
            old_no: None,
            new_no: None,
            frags: vec![DiffFrag {
                text: "⋯".to_string(),
                changed: false,
            }],
        }
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
///
/// `line_offset` is the number of file lines preceding the `old` snippet
/// (typically `start_line - 1` from `ToolOutput::Patch`). It is added to
/// every emitted line number so the gutter shows real file line numbers.
/// Pass `0` when the offset is unknown or irrelevant (e.g. `write_file`).
pub fn line_diff(old: &str, new: &str, line_offset: usize) -> Vec<DiffLine> {
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
                    old_no: Some(old_no + 1 + line_offset),
                    new_no: None,
                    frags: old_frags,
                });
                out.push(DiffLine {
                    op: DiffOp::Add,
                    old_no: None,
                    new_no: Some(new_no + 1 + line_offset),
                    frags: new_frags,
                });
            }
            for &(old_no, old_text) in del.iter().skip(pair) {
                out.push(DiffLine::plain(
                    DiffOp::Remove,
                    old_text,
                    old_no + 1 + line_offset,
                ));
            }
            for &(new_no, new_text) in ins.iter().skip(pair) {
                out.push(DiffLine::plain(
                    DiffOp::Add,
                    new_text,
                    new_no + 1 + line_offset,
                ));
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
                    let old_no = change.old_index().map(|i| i + 1 + line_offset).unwrap_or(0);
                    let new_no = change.new_index().map(|i| i + 1 + line_offset).unwrap_or(0);
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

/// Default context lines shown on each side of a change before collapsing.
const COLLAPSE_CONTEXT: usize = 4;

/// Collapse long runs of unchanged context lines into a single [`DiffOp::Ellipsis`]
/// marker so a diff spanning distant edits stays compact. Keeps up to
/// [`COLLAPSE_CONTEXT`] lines of context around each change group; everything
/// beyond that window is replaced by one `⋯` row.
///
/// Leading and trailing context (before the first / after the last change) is
/// also trimmed to `COLLAPSE_CONTEXT` lines.
pub fn collapse_context_runs(diff: &[DiffLine]) -> Vec<DiffLine> {
    let n = diff.len();
    if n == 0 {
        return Vec::new();
    }

    // Mark every line within COLLAPSE_CONTEXT of a change for keeping.
    let mut keep = vec![false; n];
    for (i, line) in diff.iter().enumerate() {
        if line.op == DiffOp::Remove || line.op == DiffOp::Add {
            for offset in 0..=COLLAPSE_CONTEXT {
                if i >= offset {
                    keep[i - offset] = true;
                }
                if i + offset < n {
                    keep[i + offset] = true;
                }
            }
        }
    }

    // Build output, inserting one ellipsis wherever a kept section follows a
    // gap of one or more skipped lines.
    let mut result = Vec::with_capacity(n);
    let mut prev_kept = false;
    let mut last_kept: Option<usize> = None;

    for (i, line) in diff.iter().enumerate() {
        if keep[i] {
            if !prev_kept && i > 0 {
                result.push(DiffLine::ellipsis());
            }
            result.push(line.clone());
            prev_kept = true;
            last_kept = Some(i);
        } else {
            prev_kept = false;
        }
    }

    // Trailing ellipsis if skipped lines follow the last kept line.
    if last_kept.is_some_and(|idx| idx + 1 < n) {
        result.push(DiffLine::ellipsis());
    }

    result
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
        let diff = line_diff("let x = 1;", "let x = 2;", 0);
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
        let diff = line_diff("a\nb\nc", "a\nB\nc", 0);
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
        let diff = line_diff(old, new, 0);
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

    #[test]
    fn line_offset_shifts_all_line_numbers() {
        // The snippet starts at file line 15, so offset = 14.
        let diff = line_diff("a\nb\nc", "a\nB\nc", 14);
        // Context line "a": file line 15 (was 1 + 14).
        assert_eq!(diff[0].op, DiffOp::Context);
        assert_eq!(diff[0].old_no, Some(15));
        assert_eq!(diff[0].new_no, Some(15));
        // Removed "b": file line 16.
        assert_eq!(diff[1].op, DiffOp::Remove);
        assert_eq!(diff[1].old_no, Some(16));
        // Added "B": file line 16.
        assert_eq!(diff[2].op, DiffOp::Add);
        assert_eq!(diff[2].new_no, Some(16));
        // Context line "c": file line 17.
        assert_eq!(diff[3].op, DiffOp::Context);
        assert_eq!(diff[3].old_no, Some(17));
    }

    #[test]
    fn collapse_inserts_ellipsis_for_long_context_runs() {
        // Two changes separated by 20 context lines.
        let old = "a\nCHANGE1\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nCHANGE2\nz";
        let new = "a\nchange1\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nchange2\nz";
        let diff = line_diff(old, new, 0);
        let collapsed = collapse_context_runs(&diff);

        // Should contain exactly one Ellipsis line.
        let ellipsis_count = collapsed
            .iter()
            .filter(|l| l.op == DiffOp::Ellipsis)
            .count();
        assert_eq!(ellipsis_count, 1, "one ellipsis for the gap");

        // The change lines survive.
        let has_remove = collapsed.iter().any(|l| l.op == DiffOp::Remove);
        let has_add = collapsed.iter().any(|l| l.op == DiffOp::Add);
        assert!(has_remove && has_add);
    }

    #[test]
    fn collapse_keeps_short_context_runs_intact() {
        // Two changes separated by only 4 context lines — within the window.
        let old = "a\nCHANGE1\nc\nd\ne\nf\nCHANGE2\nz";
        let new = "a\nchange1\nc\nd\ne\nf\nchange2\nz";
        let diff = line_diff(old, new, 0);
        let collapsed = collapse_context_runs(&diff);

        // No ellipsis needed — the gap fits within COLLAPSE_CONTEXT.
        let ellipsis_count = collapsed
            .iter()
            .filter(|l| l.op == DiffOp::Ellipsis)
            .count();
        assert_eq!(ellipsis_count, 0);
    }

    #[test]
    fn collapse_trims_leading_and_trailing_context() {
        // 20 context lines, one change, 20 more context lines.
        let old = "l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nCHANGE\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20";
        let new = "l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nchange\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20";
        let diff = line_diff(old, new, 0);
        let collapsed = collapse_context_runs(&diff);

        // Two ellipses: one before, one after the change.
        let ellipsis_count = collapsed
            .iter()
            .filter(|l| l.op == DiffOp::Ellipsis)
            .count();
        assert_eq!(ellipsis_count, 2, "leading + trailing ellipsis");
    }
}
