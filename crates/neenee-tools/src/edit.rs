use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

use crate::helpers::json_string;

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

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Apply a targeted edit to a file by replacing old_string with new_string. \
         Fails if old_string is not found."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "old_string": { "type": "string", "description": "The exact text to replace" },
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

        if !content.contains(old_str) {
            // Try whitespace-normalized match
            let normalized_content = content.replace("\r\n", "\n");
            let normalized_old = old_str.replace("\r\n", "\n");
            if let Some(match_offset) = normalized_content.find(&normalized_old) {
                let start_line = normalized_content[..match_offset].matches('\n').count() + 1;
                let (old_ctx, new_ctx, ctx_start) = contextual_patch(
                    &normalized_content,
                    match_offset,
                    &normalized_old,
                    new_str,
                    start_line,
                );
                let new_content = normalized_content.replace(&normalized_old, new_str);
                std::fs::write(path, new_content)
                    .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
                return Ok(neenee_core::ToolOutput::Patch {
                    path: path.to_string(),
                    op: neenee_core::PatchOp::Edit,
                    old: old_ctx,
                    new: new_ctx,
                    start_line: ctx_start,
                });
            }
            return Err(format!(
                "Could not find old_string in '{}'. The text may have changed or the match is ambiguous.",
                path
            ));
        }

        let match_offset = content.find(old_str).unwrap_or(0);
        let start_line = content[..match_offset].matches('\n').count() + 1;
        let (old_ctx, new_ctx, ctx_start) =
            contextual_patch(&content, match_offset, old_str, new_str, start_line);
        let new_content = content.replace(old_str, new_str);
        std::fs::write(path, new_content)
            .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
        Ok(neenee_core::ToolOutput::Patch {
            path: path.to_string(),
            op: neenee_core::PatchOp::Edit,
            old: old_ctx,
            new: new_ctx,
            start_line: ctx_start,
        })
    }
}
neenee_core::register_tool!(EditFileFactory => EditFileTool);
