use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

/// Search file contents with ripgrep.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search for a regex pattern in files using ripgrep. Returns matches \
         in `path:line:content` format with 2 lines of surrounding context."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Directory or file to search in (default '.')" },
                "ext": { "type": "string", "description": "Optional file extension filter (e.g., 'rs', 'py')" }
            },
            "required": ["pattern"]
        })
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let pattern = args["pattern"].as_str().ok_or("Missing 'pattern'")?;
        let path = args["path"].as_str().unwrap_or(".");
        let ext = args["ext"].as_str();

        let mut cmd = std::process::Command::new("rg");
        cmd.args(["-n", "--color=never", "--max-count", "50", "-C", "2"]);
        if let Some(e) = ext {
            cmd.arg("-g").arg(format!("*.{}", e));
        }
        for dir in [".git", "node_modules", "target", "__pycache__"] {
            cmd.arg("-g").arg(format!("!{}", dir));
        }
        cmd.arg(pattern).arg(path);

        let output = cmd
            .output()
            .map_err(|e| format!("Failed to run rg: {}. Is ripgrep installed?", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if stdout.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if !stderr.is_empty() {
                return Err(format!("rg error: {}", stderr));
            }
            return Ok("No matches found.".to_string());
        }
        Ok(stdout)
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
        let out = self.call(arguments).await?;
        let pattern = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|a| a["pattern"].as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(neenee_core::ToolOutput::Matches {
            pattern,
            lines: out.split('\n').map(str::to_string).collect(),
        })
    }
}
neenee_core::register_tool!(GrepFactory => GrepTool);
