//! Line-level diff used to visualize `edit_file` / `write_file` changes.
//!
//! `edit_file` replaces a contiguous `old_string` with `new_string`, and
//! `write_file` writes whole new content — so a full LCS diff is overkill.
//! We trim common leading/trailing lines to bounded context and show the
//! differing middle as removed-then-added, which matches how the model
//! actually edits and keeps the rendered diff compact.

/// Bounded number of unchanged context lines shown on each side of a change.
const CONTEXT_LINES: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffOp {
    /// Unchanged line shown for context.
    Context,
    /// Line present only in the new text.
    Add,
    /// Line present only in the old text.
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

/// Count of common leading (`prefix`) and trailing (`suffix`) lines shared by
/// `old` and `new`, with the suffix bounded so it never overlaps the prefix.
fn common_bounds(old: &[&str], new: &[&str]) -> (usize, usize) {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    (prefix, suffix)
}

/// `(added, removed)` line counts for the change from `old` to `new`, after
/// trimming shared prefix/suffix. Used for the `+N -M` summary suffix.
pub fn line_diff_counts(old: &str, new: &str) -> (usize, usize) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let (prefix, suffix) = common_bounds(&old_lines, &new_lines);
    let added = new_lines.len().saturating_sub(prefix + suffix);
    let removed = old_lines.len().saturating_sub(prefix + suffix);
    (added, removed)
}

/// Build the renderable diff: bounded leading context, the removed middle,
/// the added middle, then bounded trailing context. Collapsed-context runs are
/// summarized with a `… N unchanged` marker so large unchanged regions stay
/// compact.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let (prefix, suffix) = common_bounds(&old_lines, &new_lines);

    let mut out = Vec::new();
    let ctx = |text: String| DiffLine {
        op: DiffOp::Context,
        text,
    };

    // Leading context: only the last CONTEXT_LINES of the shared prefix.
    let lead_shown = prefix.min(CONTEXT_LINES);
    if prefix > lead_shown {
        out.push(ctx(format!("… {} unchanged", prefix - lead_shown)));
    }
    for line in &old_lines[prefix - lead_shown..prefix] {
        out.push(ctx((*line).to_string()));
    }

    // Differing middle.
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        out.push(DiffLine {
            op: DiffOp::Remove,
            text: (*line).to_string(),
        });
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        out.push(DiffLine {
            op: DiffOp::Add,
            text: (*line).to_string(),
        });
    }

    // Trailing context: only the first CONTEXT_LINES of the shared suffix.
    let suffix_start = old_lines.len() - suffix;
    let tail_shown = suffix.min(CONTEXT_LINES);
    for line in &old_lines[suffix_start..suffix_start + tail_shown] {
        out.push(ctx((*line).to_string()));
    }
    if suffix > tail_shown {
        out.push(ctx(format!("… {} unchanged", suffix - tail_shown)));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_ignore_shared_prefix_and_suffix() {
        let old = "a\nb\nc\nd";
        let new = "a\nB\nc\nd";
        assert_eq!(line_diff_counts(old, new), (1, 1));
    }

    #[test]
    fn write_from_scratch_is_all_added() {
        assert_eq!(line_diff_counts("", "x\ny\nz"), (3, 0));
    }

    #[test]
    fn diff_shows_removed_then_added_with_context() {
        let old = "keep\nold line\ntail";
        let new = "keep\nnew line\ntail";
        let diff = line_diff(old, new);
        let ops: Vec<_> = diff.iter().map(|d| (d.op, d.text.as_str())).collect();
        assert_eq!(
            ops,
            vec![
                (DiffOp::Context, "keep"),
                (DiffOp::Remove, "old line"),
                (DiffOp::Add, "new line"),
                (DiffOp::Context, "tail"),
            ]
        );
    }

    #[test]
    fn large_unchanged_regions_collapse() {
        let old = "1\n2\n3\n4\n5\nX\n6\n7\n8\n9\n10";
        let new = "1\n2\n3\n4\n5\nY\n6\n7\n8\n9\n10";
        let diff = line_diff(old, new);
        // Leading context collapsed to a marker + 3 lines.
        assert_eq!(diff[0].op, DiffOp::Context);
        assert!(diff[0].text.contains("unchanged"));
    }
}
