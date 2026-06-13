use crate::{Goal, GoalChecklistItem, GoalChecklistStatus, GoalStatus, Tool, ToolAccess};
use async_trait::async_trait;
use serde_json::json;
use std::process::Command;
use std::sync::{Arc, Mutex};

pub struct GoalChecklistTool {
    goal: Arc<Mutex<Option<Goal>>>,
}

impl GoalChecklistTool {
    pub(crate) fn new(goal: Arc<Mutex<Option<Goal>>>) -> Self {
        Self { goal }
    }
}

#[async_trait]
impl Tool for GoalChecklistTool {
    fn name(&self) -> &str {
        "goal_checklist"
    }

    fn description(&self) -> &str {
        "Replace the active goal's structured checklist. Use this to expose concrete progress. \
         Keep exactly one item in_progress while working; mark verified work completed."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": 50,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Arguments {
            items: Vec<GoalChecklistItem>,
        }

        let arguments: Arguments =
            serde_json::from_str(arguments).map_err(|error| format!("Invalid JSON: {}", error))?;
        if arguments.items.len() > 50 {
            return Err("Goal checklist is limited to 50 items.".to_string());
        }
        if arguments
            .items
            .iter()
            .any(|item| item.content.trim().is_empty())
        {
            return Err("Goal checklist item content cannot be empty.".to_string());
        }
        let in_progress = arguments
            .items
            .iter()
            .filter(|item| item.status == GoalChecklistStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err("At most one goal checklist item may be in_progress.".to_string());
        }

        let mut goal = self.goal.lock().unwrap_or_else(|error| error.into_inner());
        let goal = goal
            .as_mut()
            .ok_or_else(|| "No active goal. Set one with /goal <objective>.".to_string())?;
        if goal.status != GoalStatus::Active {
            return Err("The goal is already completed.".to_string());
        }
        if arguments.items.is_empty() && !goal.checklist.is_empty() {
            return Err(
                "An active checklist cannot be cleared. Mark each item completed or cancelled."
                    .to_string(),
            );
        }
        goal.checklist = arguments.items;
        let resolved = goal
            .checklist
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                )
            })
            .count();
        Ok(format!(
            "Goal checklist updated: {}/{} resolved.",
            resolved,
            goal.checklist.len()
        ))
    }
}

/// Read a file from disk.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the full contents of a file. Use this when you need to see code or text content."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or relative path to the file" },
                "offset": { "type": "integer", "description": "Optional line offset (1-based) to start reading from" },
                "limit": { "type": "integer", "description": "Optional max number of lines to read" }
            },
            "required": ["path"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let path = args["path"].as_str().ok_or("Missing 'path'")?;

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read '{}': {}", path, e))?;

        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(0) as usize;

        let lines: Vec<&str> = content.lines().collect();
        let start = offset - 1;
        let end = if limit > 0 {
            (start + limit).min(lines.len())
        } else {
            lines.len()
        };

        if start >= lines.len() {
            return Ok(String::new());
        }
        let slice = &lines[start..end];
        let result = slice.join("\n");

        // Offload very large outputs
        if result.len() > 8000 {
            return Ok(format!(
                "[Output truncated: {} lines, {} chars total]\n{}\n\n[Use offset/limit or read_file to see more]",
                lines.len(),
                content.len(),
                &result[..4000]
            ));
        }
        Ok(result)
    }
}

/// Write content to a file (overwrites).
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        })
    }
    fn permission_scope(&self, arguments: &str) -> String {
        json_string(arguments, "path")
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let path = args["path"].as_str().ok_or("Missing 'path'")?;
        let content = args["content"].as_str().ok_or("Missing 'content'")?;

        // Create parent directories if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create dirs for '{}': {}", path, e))?;
        }

        std::fs::write(path, content).map_err(|e| format!("Failed to write '{}': {}", path, e))?;
        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path
        ))
    }
}

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
    fn permission_scope(&self, arguments: &str) -> String {
        json_string(arguments, "path")
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
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
                let new_content = normalized_content.replace(&normalized_old, new_str);
                std::fs::write(path, new_content)
                    .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
                return Ok(format!(
                    "Edited '{}' (matched with whitespace normalization)",
                    path
                ));
            }
            return Err(format!(
                "Could not find old_string in '{}'. The text may have changed or the match is ambiguous.",
                path
            ));
        }

        let new_content = content.replace(old_str, new_str);
        std::fs::write(path, new_content)
            .map_err(|e| format!("Failed to write '{}': {}", path, e))?;
        Ok(format!("Edited '{}' successfully", path))
    }
}

/// Execute a bash command.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a shell command. Use for git, build, test, or any system operation."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout": { "type": "integer", "description": "Timeout in seconds (default 30)" }
            },
            "required": ["command"]
        })
    }
    fn permission_scope(&self, arguments: &str) -> String {
        json_string(arguments, "command")
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let command = args["command"].as_str().ok_or("Missing 'command'")?;
        let _timeout_secs = args["timeout"].as_u64().unwrap_or(30);

        let output = if cfg!(target_os = "windows") {
            Command::new("cmd").args(["/C", command]).output()
        } else {
            Command::new("sh").arg("-c").arg(command).output()
        }
        .map_err(|e| format!("Failed to execute: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        let result = if output.status.success() {
            if stdout.is_empty() && !stderr.is_empty() {
                format!("(success, stderr):\n{}", stderr)
            } else {
                stdout
            }
        } else {
            format!(
                "Exit {}\nSTDOUT:\n{}\nSTDERR:\n{}",
                output.status.code().unwrap_or(-1),
                stdout,
                stderr
            )
        };

        // Truncate large outputs
        if result.len() > 8000 {
            return Ok(format!(
                "[Output truncated: {} chars total]\n{}\n\n[Output was large — use grep or read_file if you need specific parts]",
                result.len(),
                &result[..4000]
            ));
        }
        Ok(result)
    }
}

fn json_string(arguments: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| value.get(key)?.as_str().map(str::to_string))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "*".to_string())
}

/// Search file contents with ripgrep.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search for a regex pattern in files. Uses ripgrep for speed."
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
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let pattern = args["pattern"].as_str().ok_or("Missing 'pattern'")?;
        let path = args["path"].as_str().unwrap_or(".");
        let ext = args["ext"].as_str();

        let mut cmd = Command::new("rg");
        cmd.args(["-n", "--color=never", "--max-count", "50", "-C", "2"]);
        if let Some(e) = ext {
            cmd.arg("-g").arg(format!("*.{}", e));
        }
        cmd.args([
            "--exclude-dir=.git",
            "--exclude-dir=node_modules",
            "--exclude-dir=target",
            "--exclude-dir=__pycache__",
        ]);
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
}

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
        ToolAccess::ReadOnly
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
                let meta = entry.metadata();
                let prefix = match meta {
                    Ok(m) if m.is_dir() => "📁",
                    Ok(m) if m.is_file() => "📄",
                    _ => "?",
                };
                results.push(format!("{} {}", prefix, name));
            }
        }

        if results.is_empty() {
            Ok("No files found.".to_string())
        } else {
            Ok(results.join("\n"))
        }
    }
}

/// Use a skill (loads skill content into context).
pub struct UseSkillTool {
    pub skills: std::sync::Arc<std::sync::Mutex<Vec<crate::skills::Skill>>>,
}

#[async_trait]
impl Tool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }
    fn description(&self) -> &str {
        "Load a skill into the conversation context. Skills provide domain-specific expertise. \
         Call this when the task matches a skill's description."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The skill name to load" }
            },
            "required": ["name"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let name = args["name"].as_str().ok_or("Missing 'name'")?;

        let skills = self
            .skills
            .lock()
            .map_err(|e| format!("Lock error: {}", e))?;
        for skill in skills.iter() {
            if skill.name == name {
                return Ok(format!(
                    "[Skill '{}' loaded]\n{}\n[/Skill]",
                    skill.name, skill.content
                ));
            }
        }
        Err(format!(
            "Skill '{}' not found. Available skills can be discovered via the system prompt.",
            name
        ))
    }
}
