use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

use crate::helpers::json_string;

/// Apply an edit to a file (safer than write_file — requires old_string match).
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Apply a targeted edit to a file. Replaces old_string with new_string. \
         This is safer than write_file because it verifies the content exists. \
         If old_string is not found, the tool fails."
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
            if normalized_content.contains(&normalized_old) {
                let start_line = normalized_content
                    .find(&normalized_old)
                    .map(|offset| normalized_content[..offset].matches('\n').count() + 1)
                    .unwrap_or(0);
                let new_content = normalized_content.replace(&normalized_old, new_str);
                std::fs::write(path, new_content)
                    .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
                return Ok(neenee_core::ToolOutput::Patch {
                    path: path.to_string(),
                    op: neenee_core::PatchOp::Edit,
                    old: old_str.to_string(),
                    new: new_str.to_string(),
                    start_line,
                });
            }
            return Err(format!(
                "Could not find old_string in '{}'. The text may have changed or the match is ambiguous.",
                path
            ));
        }

        let start_line = content
            .find(old_str)
            .map(|offset| content[..offset].matches('\n').count() + 1)
            .unwrap_or(0);
        let new_content = content.replace(old_str, new_str);
        std::fs::write(path, new_content)
            .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
        Ok(neenee_core::ToolOutput::Patch {
            path: path.to_string(),
            op: neenee_core::PatchOp::Edit,
            old: old_str.to_string(),
            new: new_str.to_string(),
            start_line,
        })
    }
}
neenee_core::register_tool!(EditFileFactory => EditFileTool);
