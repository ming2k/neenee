use crate::Tool;
use serde_json::json;
use std::process::Command;
use async_trait::async_trait;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return the output"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Invalid JSON arguments: {}", e))?;
        
        let command = args["command"].as_str()
            .ok_or_else(|| "Missing 'command' argument".to_string())?;

        let output = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(["/C", command])
                .output()
        } else {
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
        }.map_err(|e| format!("Failed to execute command: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if output.status.success() {
            Ok(stdout)
        } else {
            Err(format!("Command failed with status {}:\nSTDOUT: {}\nSTDERR: {}", output.status, stdout, stderr))
        }
    }
}

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Invalid JSON arguments: {}", e))?;
        
        let path = args["path"].as_str()
            .ok_or_else(|| "Missing 'path' argument".to_string())?;

        std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read file {}: {}", path, e))
    }
}

pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Invalid JSON arguments: {}", e))?;
        
        let path = args["path"].as_str()
            .ok_or_else(|| "Missing 'path' argument".to_string())?;
        let content = args["content"].as_str()
            .ok_or_else(|| "Missing 'content' argument".to_string())?;

        std::fs::write(path, content)
            .map_err(|e| format!("Failed to write to file {}: {}", path, e))?;
        
        Ok(format!("Successfully wrote to {}", path))
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search for a pattern in files within a directory"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in (defaults to current directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Invalid JSON arguments: {}", e))?;
        
        let pattern = args["pattern"].as_str()
            .ok_or_else(|| "Missing 'pattern' argument".to_string())?;
        let path = args["path"].as_str().unwrap_or(".");

        let output = Command::new("grep")
            .args(["-r", "-n", "--exclude-dir=.git", pattern, path])
            .output()
            .map_err(|e| format!("Failed to execute grep: {}", e))?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "List files matching a glob pattern"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern (e.g., 'src/**/*.rs')"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|e| format!("Invalid JSON arguments: {}", e))?;
        
        let pattern = args["pattern"].as_str()
            .ok_or_else(|| "Missing 'pattern' argument".to_string())?;

        // Using 'find' as a fallback for 'glob' since we want to stay vanilla-ish
        let output = Command::new("find")
            .args([".", "-path", pattern, "-not", "-path", "*/.*"])
            .output()
            .map_err(|e| format!("Failed to execute find: {}", e))?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}
