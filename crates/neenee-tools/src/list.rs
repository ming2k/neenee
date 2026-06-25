use async_trait::async_trait;
use neenee_core::{Tool, ToolAccess};
use serde_json::json;

/// List directory contents.
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List files and directories. Supports glob patterns."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to list (default '.')" },
                "pattern": { "type": "string", "description": "Optional glob pattern to filter results (e.g., '*.rs')" },
                "recursive": { "type": "boolean", "description": "Whether to list recursively (default false)" },
                "max_results": { "type": "integer", "description": "Max entries to return (default 100)" }
            },
            "required": []
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let path = args["path"].as_str().unwrap_or(".");
        let pattern = args["pattern"].as_str();
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let max_results = args["max_results"].as_u64().unwrap_or(100) as usize;

        let mut results = Vec::new();
        let _base = std::path::Path::new(path);

        if let Some(glob_pat) = pattern {
            let full_pattern = if recursive {
                format!("{}/**/{}\0{}/{}", path, glob_pat, path, glob_pat)
            } else {
                format!("{}/{}\0{}/{}", path, glob_pat, path, glob_pat)
            };
            // Split and deduplicate
            let patterns: Vec<&str> = full_pattern.split('\0').collect();
            for pat in patterns {
                for entry in glob::glob(pat).map_err(|e| format!("Bad glob pattern: {}", e))? {
                    let path = entry.map_err(|e| format!("Glob error: {}", e))?;
                    if results.len() >= max_results {
                        break;
                    }
                    let display = path
                        .strip_prefix(std::env::current_dir().unwrap_or_default())
                        .unwrap_or(&path);
                    results.push(display.to_string_lossy().to_string());
                }
                if results.len() >= max_results {
                    break;
                }
            }
        } else if recursive {
            for entry in walkdir::WalkDir::new(path)
                .max_depth(10)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if results.len() >= max_results {
                    break;
                }
                let p = entry.path();
                if p.file_name()
                    .map(|n| n.to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
                {
                    continue;
                }
                let display = p
                    .strip_prefix(std::env::current_dir().unwrap_or_default())
                    .unwrap_or(p);
                results.push(display.to_string_lossy().to_string());
            }
        } else {
            let entries = std::fs::read_dir(path)
                .map_err(|e| format!("Failed to read dir '{}': {}", path, e))?;
            for entry in entries.filter_map(|e| e.ok()) {
                if results.len() >= max_results {
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
                // Unix-style `ls -p` convention: directories get a trailing
                // slash so they're visually distinct from files at a glance,
                // without relying on emoji that may not render everywhere.
                let suffix = if is_dir { "/" } else { "" };
                results.push(format!("{}{}", name, suffix));
            }
        }

        if results.is_empty() {
            Ok("No files found.".to_string())
        } else {
            Ok(results.join("\n"))
        }
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
        let out = self.call(arguments).await?;
        Ok(neenee_core::ToolOutput::Listing {
            entries: out.split('\n').map(str::to_string).collect(),
        })
    }
}
neenee_core::register_tool!(ListDirFactory => ListDirTool);
