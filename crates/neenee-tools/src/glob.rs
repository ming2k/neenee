use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

/// Fast file pattern matching using globs.
pub struct GlobTool;

const GLOB_MAX_RESULTS: usize = 200;

fn glob_should_skip(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(
                ".git"
                    | "node_modules"
                    | "target"
                    | "__pycache__"
                    | ".next"
                    | "dist"
                    | "build"
                    | ".venv"
                    | "venv"
                    | ".cache"
            )
        )
    })
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern (e.g., '**/*.rs', 'src/**/*.ts'). Returns matching paths."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. '**/*.rs', 'docs/*.md')" },
                "path": { "type": "string", "description": "Base directory to search from (default '.')" }
            },
            "required": ["pattern"]
        })
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let pattern = args["pattern"].as_str().ok_or("Missing 'pattern'")?;
        let base = args["path"].as_str().unwrap_or(".");

        let base_path = std::path::Path::new(base);
        let candidates = if pattern.contains('/') || base != "." {
            vec![base_path.join(pattern).to_string_lossy().to_string()]
        } else {
            vec![
                base_path.join(pattern).to_string_lossy().to_string(),
                base_path
                    .join("**")
                    .join(pattern)
                    .to_string_lossy()
                    .to_string(),
            ]
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for candidate in &candidates {
            for entry in glob::glob(candidate).map_err(|e| format!("Bad glob pattern: {}", e))? {
                let path = match entry {
                    Ok(path) => path,
                    Err(_) => continue,
                };
                if glob_should_skip(&path) {
                    continue;
                }
                let display = path
                    .strip_prefix(&cwd)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if seen.insert(display.clone()) {
                    results.push(display);
                }
                if results.len() >= GLOB_MAX_RESULTS {
                    break;
                }
            }
            if results.len() >= GLOB_MAX_RESULTS {
                break;
            }
        }

        if results.is_empty() {
            Ok("No files matched the pattern.".to_string())
        } else {
            Ok(results.join("\n"))
        }
    }
}
neenee_core::register_tool!(GlobFactory => GlobTool);
