use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

use crate::helpers::{json_string, save_file_atomic};

/// Apply an edit to a file (safer than write_file — requires old_string match).
pub struct EditFileTool;

/// Number of unchanged context lines to include above and below the edit in the
/// diff display (GitHub-style: 3 lines of surrounding context).
const DIFF_CONTEXT: usize = 3;

/// Extract up to [`DIFF_CONTEXT`] lines above and below a match in `content`,
/// returning the context-bracketed `old`/`new` snippets and an adjusted
/// `start_line` so the line-number gutter reflects true file positions.
fn contextual_patch(
    content: &str,
    match_offset: usize,
    old_str: &str,
    new_str: &str,
    start_line: usize,
) -> (String, String, usize) {
    let before = &content[..match_offset];
    let after = &content[match_offset + old_str.len()..];

    let before_lines: Vec<&str> = before
        .lines()
        .rev()
        .take(DIFF_CONTEXT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after_lines: Vec<&str> = after.lines().take(DIFF_CONTEXT).collect();

    let new_start_line = start_line.saturating_sub(before_lines.len()).max(1);

    let build = |replacement: &str| -> String {
        let mut s = String::with_capacity(
            before_lines.iter().map(|l| l.len() + 1).sum::<usize>()
                + replacement.len()
                + after_lines.iter().map(|l| l.len() + 1).sum::<usize>(),
        );
        for l in &before_lines {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str(replacement);
        for l in &after_lines {
            s.push('\n');
            s.push_str(l);
        }
        s
    };

    (build(old_str), build(new_str), new_start_line)
}

/// Count non-overlapping occurrences of `needle` in `haystack`.
///
/// An empty `needle` reports zero matches — an empty `old_string` is never a
/// valid edit, and `str::matches("")` would otherwise enumerate every inter-char
/// position and overflow.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

/// The components needed to display and persist a successful edit.
#[derive(Debug)]
struct AppliedEdit {
    old_ctx: String,
    new_ctx: String,
    ctx_start: usize,
    new_content: String,
}

/// Find a **unique** match of `old` in `content`, build the contextual diff
/// patch, and produce replacement content with exactly that one occurrence
/// swapped for `new`.
///
/// Return value:
/// - `Ok(Some(_))` — exactly one match; safe to apply.
/// - `Ok(None)` — no match (caller may try a fallback or report not-found).
/// - `Err(_)` — the match is *ambiguous* (`count > 1`). This is an error, never
///   a silent global replace: an edit intended for one site must not rewrite
///   every look-alike occurrence.
fn apply_unique_edit(
    content: &str,
    old: &str,
    new: &str,
    path: &str,
) -> Result<Option<AppliedEdit>, String> {
    match count_occurrences(content, old) {
        0 => Ok(None),
        1 => {
            // `find` is guaranteed to return `Some` here (count is exactly 1),
            // but guard with `let … else` so the function stays panic-free even
            // if the invariant above is ever weakened.
            let Some(offset) = content.find(old) else {
                return Ok(None);
            };
            let start_line = content[..offset].matches('\n').count() + 1;
            let (old_ctx, new_ctx, ctx_start) =
                contextual_patch(content, offset, old, new, start_line);
            // Replace only this single occurrence by stitching the prefix, the
            // new text, and the suffix back together — *not* `str::replace`,
            // which would rewrite every occurrence.
            let mut new_content = String::with_capacity(content.len() - old.len() + new.len());
            new_content.push_str(&content[..offset]);
            new_content.push_str(new);
            new_content.push_str(&content[offset + old.len()..]);
            Ok(Some(AppliedEdit {
                old_ctx,
                new_ctx,
                ctx_start,
                new_content,
            }))
        }
        n => Err(format!(
            "old_string matches {n} places in '{path}'. Add more surrounding context so the match is unique."
        )),
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Apply a targeted edit to a file by replacing old_string with new_string. \
         The old_string must appear exactly once; fails if not found or if the \
         match is ambiguous."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "old_string": { "type": "string", "description": "The exact text to replace; must be unique in the file" },
                "new_string": { "type": "string", "description": "The replacement text" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn scope_target(&self, arguments: &str) -> neenee_core::ScopeTarget {
        neenee_core::ScopeTarget::Path(std::path::PathBuf::from(json_string(arguments, "path")))
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let path = args["path"].as_str().ok_or("Missing 'path'")?;
        let old_str = args["old_string"].as_str().ok_or("Missing 'old_string'")?;
        let new_str = args["new_string"].as_str().ok_or("Missing 'new_string'")?;

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read '{}': {}", path, e))?;

        // Exact match first; fall back to a CRLF-normalized comparison so an
        // edit authored with LF line endings works against a CRLF file. Either
        // path requires the match to be *unique* — an ambiguous old_string is an
        // error, never a silent global replace.
        let edit = match apply_unique_edit(&content, old_str, new_str, path)? {
            Some(e) => e,
            None => {
                let normalized_content = content.replace("\r\n", "\n");
                let normalized_old = old_str.replace("\r\n", "\n");
                match apply_unique_edit(&normalized_content, &normalized_old, new_str, path)? {
                    Some(e) => e,
                    None => {
                        return Err(format!(
                            "Could not find old_string in '{}'. The text may have changed or the match is ambiguous.",
                            path
                        ));
                    }
                }
            }
        };

        // Atomically commit the new content (temp file + fsync + rename) so an
        // interrupted edit never corrupts the file in place.
        save_file_atomic(std::path::Path::new(path), edit.new_content.as_bytes())
            .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
        Ok(neenee_core::ToolOutput::Patch {
            path: path.to_string(),
            op: neenee_core::PatchOp::Edit,
            old: edit.old_ctx,
            new: edit.new_ctx,
            start_line: edit.ctx_start,
        })
    }
}
neenee_core::register_tool!(EditFileFactory => EditFileTool);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_occurrences_is_zero_for_empty_needle() {
        assert_eq!(count_occurrences("abc", ""), 0);
    }

    #[test]
    fn count_occurrences_handles_overlapping_and_repeats() {
        assert_eq!(count_occurrences("aaa", "a"), 3);
        // `str::matches` is non-overlapping, so "aaaa"/"aa" is 2, not 3.
        assert_eq!(count_occurrences("aaaa", "aa"), 2);
        assert_eq!(count_occurrences("abcabc", "abc"), 2);
        assert_eq!(count_occurrences("abc", "z"), 0);
    }

    #[test]
    fn apply_unique_edit_replaces_exactly_one_occurrence() {
        let content = "foo\nbar\nbaz".to_string();
        let edit = apply_unique_edit(&content, "bar", "qux", "f.txt")
            .unwrap()
            .expect("single match should apply");
        assert_eq!(edit.new_content, "foo\nqux\nbaz");
    }

    #[test]
    fn apply_unique_edit_errors_on_ambiguous_match() {
        let content = "dup\ndup\nother".to_string();
        let err = apply_unique_edit(&content, "dup", "x", "f.txt").unwrap_err();
        assert!(
            err.contains("2 places"),
            "ambiguous match must report count: {err}"
        );
    }

    #[test]
    fn apply_unique_edit_returns_none_when_absent() {
        let content = "hello world".to_string();
        assert!(
            apply_unique_edit(&content, "goodbye", "x", "f.txt")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn apply_unique_edit_replaces_between_occurrences() {
        // The replacement must not touch the second occurrence (regression for
        // the old `str::replace`-replaces-all behaviour).
        let content = "x KEEP x".to_string();
        let edit = apply_unique_edit(&content, "x", "Y", "f.txt").unwrap_err(); // ambiguous (2 occurrences) -> error
        assert!(edit.contains("2 places"));
    }
}
