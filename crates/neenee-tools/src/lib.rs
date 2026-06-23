use async_trait::async_trait;
use neenee_core::{truncate_utf8, Tool, ToolAccess, WebSearchConfig};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub mod commands;
pub mod mcp;
pub mod project;
pub mod search;

use search::SearchProvider;

/// Read a file from disk.
pub struct ReadFileTool;

/// Ask the user one or more multiple-choice questions mid-task.
///
/// The actual blocking user interaction is handled by the agent harness (see
/// `Agent::execute_tool`). The tool implementation itself only provides the
/// schema and description exposed to the model.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user one or more multiple-choice questions to clarify preferences, resolve ambiguity, or decide between trade-offs. \
         Use this when the request is vague, when multiple valid approaches exist, or before a risky/destructive action. \
         Provide 2-4 labeled options per question; put the recommended option first and suffix its label with '(Recommended)'. \
         The user can always choose 'Other' and type a free-form answer."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to ask the user. Each question is presented in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "description": "Very short label displayed as a chip/tag for the question (optional)."
                            },
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask the user."
                            },
                            "options": {
                                "type": "array",
                                "description": "Available choices. Provide 2-4 options. Put the recommended option first and suffix its label with '(Recommended)'.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Short choice label returned to you if selected."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Optional longer explanation of the choice."
                                        }
                                    },
                                    "required": ["label"]
                                },
                                "minItems": 1
                            },
                            "multi_select": {
                                "type": "boolean",
                                "default": false,
                                "description": "Whether the user may select more than one option."
                            }
                        },
                        "required": ["question", "options"]
                    },
                    "minItems": 1,
                    "maxItems": 5
                }
            },
            "required": ["questions"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    /// `ask_user` blocks on a live human answer; sub-agents (which have no
    /// user reachable) must be excluded from it by their profile.
    fn requires_user(&self) -> bool {
        true
    }

    fn allowed_in_plan_mode(&self, _arguments: &str) -> bool {
        true
    }

    async fn call(&self, _arguments: &str) -> Result<String, String> {
        Err(
            "ask_user is handled by the agent harness and should not be called directly"
                .to_string(),
        )
    }
}

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
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
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

        let lang = std::path::Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_string());
        if start >= lines.len() {
            return Ok(neenee_core::ToolOutput::Code {
                lang,
                text: String::new(),
            });
        }
        let slice = &lines[start..end];
        let result = slice.join("\n");

        // Offload very large outputs
        let text = if result.len() > 8000 {
            format!(
                "[Output truncated: {} lines, {} chars total]\n{}\n\n[Use offset/limit or read_file to see more]",
                lines.len(),
                content.len(),
                truncate_utf8(&result, 4000)
            )
        } else {
            result
        };
        Ok(neenee_core::ToolOutput::Code { lang, text })
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
    fn allowed_in_plan_mode(&self, arguments: &str) -> bool {
        neenee_core::plan::is_plan_path(&json_string(arguments, "path"))
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
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
        Ok(neenee_core::ToolOutput::Patch {
            path: path.to_string(),
            op: neenee_core::PatchOp::Create,
            old: String::new(),
            new: content.to_string(),
            start_line: 0,
        })
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
    fn allowed_in_plan_mode(&self, arguments: &str) -> bool {
        neenee_core::plan::is_plan_path(&json_string(arguments, "path"))
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

/// Execute a bash command.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    /// `bash` runs commands — its primary purpose is execution, not workspace
    /// mutation — so it sits in the `Execute` tier between pure reads and
    /// file-writing tools. The broker still gates it (`Execute > Read`), and
    /// it is the tier the `VERIFY` sub-agent profile admits so an independent
    /// verifier can run tests/builds/type-checks without gaining file-write
    /// capability. See ADR-0012.
    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
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
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
        // Non-streaming path: delegate with no-op sinks.
        self.call_structured_with_events("", arguments, Box::new(|_| {}), &mut |_| {})
            .await
    }

    /// Spawn the command with piped stdout/stderr, stream stdout line-by-line
    /// as it arrives, and drain stderr concurrently (so a full stderr pipe
    /// can't deadlock the child while we read stdout). The `&mut` stream sink
    /// can't cross a spawned task boundary, so stderr is accumulated rather
    /// than streamed live; stdout — the primary channel — streams live.
    async fn call_structured_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(neenee_core::SubTaskEvent) + Send + 'a>,
        on_stream: &mut (dyn FnMut(neenee_core::ToolStream) + Send + 'a),
    ) -> Result<neenee_core::ToolOutput, String> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let command = args["command"].as_str().ok_or("Missing 'command'")?;
        let timeout_secs = args["timeout"].as_u64().unwrap_or(30);
        let timeout_duration = Duration::from_secs(timeout_secs);

        let mut child = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(["/C", command])
                .kill_on_drop(true)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        } else {
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .kill_on_drop(true)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        }
        .map_err(|e| format!("Failed to execute: {}", e))?;

        let stdout = child
            .stdout
            .take()
            .ok_or("failed to capture child stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("failed to capture child stderr")?;

        // Drain stderr on a separate task so the child can't block on a full
        // stderr pipe while the main task reads stdout.
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        // `kill_on_drop` guarantees the child is terminated when this future is
        // dropped — on timeout (the `Timeout` wrapper drops the inner future)
        // and on mid-run interrupt (see `execute_tools_concurrent`).
        let run = async {
            let mut stdout_buf = String::new();
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                stdout_buf.push_str(&line);
                stdout_buf.push('\n');
                on_stream(neenee_core::ToolStream::Stdout(format!("{}\n", line)));
            }
            let stderr_buf = stderr_task.await.unwrap_or_default();
            let status = child
                .wait()
                .await
                .map_err(|e| format!("Failed to wait: {}", e))?;
            let exit = status.code();
            let truncated =
                neenee_core::tool_output::shell_inner_text(&stdout_buf, &stderr_buf, exit).len()
                    > 8000;
            Ok(neenee_core::ToolOutput::Shell {
                command: command.to_string(),
                stdout: stdout_buf,
                stderr: stderr_buf,
                exit,
                truncated,
            }) as Result<neenee_core::ToolOutput, String>
        };

        timeout(timeout_duration, run)
            .await
            .map_err(|_| format!("Command timed out after {} seconds", timeout_secs))?
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
        ToolAccess::Read
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
        "Fast file pattern matching. Find files by glob pattern, e.g. '**/*.rs' or \
         'src/**/*.ts'. Returns matching paths. Prefer this over recursive listing when you \
         know the file extension or naming pattern."
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
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
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

/// Fetch a URL and return its text content (HTML stripped to text).
pub struct WebFetchTool {
    config: Arc<WebSearchConfig>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            config: Arc::new(WebSearchConfig::default()),
        }
    }
    pub fn with_config(config: WebSearchConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the shared HTTP client honoring the web tools' proxy and timeout.
fn http_client(config: &WebSearchConfig) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
        .user_agent("neenee/0.1 (+ai-coding-agent)");
    if let Some(proxy_url) = config
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|e| format!("Invalid proxy '{}': {}", proxy_url, e))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// Naive HTML → text conversion. Collapses whitespace and strips tags/scripts.
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip = false;
    let lower = html.to_ascii_lowercase();
    let mut chars = html.char_indices().peekable();
    while let Some((byte_idx, c)) = chars.next() {
        if !in_tag && lower[byte_idx..].starts_with("<script") {
            skip = true;
        } else if skip && lower[byte_idx..].starts_with("</script") {
            skip = false;
            // jump to end of tag
            if let Some(idx) = lower[byte_idx..].find('>') {
                let next_byte = byte_idx + idx + 1;
                while chars
                    .peek()
                    .is_some_and(|(peek_byte, _)| *peek_byte < next_byte)
                {
                    chars.next();
                }
                continue;
            }
        } else if !in_tag && lower[byte_idx..].starts_with("<style") {
            skip = true;
        } else if skip && lower[byte_idx..].starts_with("</style") {
            skip = false;
            if let Some(idx) = lower[byte_idx..].find('>') {
                let next_byte = byte_idx + idx + 1;
                while chars
                    .peek()
                    .is_some_and(|(peek_byte, _)| *peek_byte < next_byte)
                {
                    chars.next();
                }
                continue;
            }
        }
        if skip {
            continue;
        }
        if c == '<' {
            in_tag = true;
        } else if c == '>' && in_tag {
            in_tag = false;
            out.push(' ');
        } else if !in_tag {
            out.push(c);
        }
    }
    // Decode a handful of common entities
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(decoded.len());
    let mut prev_ws = false;
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
                prev_ws = true;
            }
        } else {
            collapsed.push(c);
            prev_ws = false;
        }
    }
    collapsed.trim().to_string()
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn description(&self) -> &str {
        "Fetch the content of a web page or URL and return it as text. Use for reading \
         documentation, APIs, or any publicly accessible resource. HTML pages are converted to \
         plain text. Output is truncated for very large pages."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The fully-qualified URL to fetch (http/https)" },
                "raw": { "type": "boolean", "description": "If true, return raw content without HTML stripping (default false)" }
            },
            "required": ["url"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let url = args["url"].as_str().ok_or("Missing 'url'")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("URL must start with http:// or https://".to_string());
        }
        let raw = args["raw"].as_bool().unwrap_or(false);
        let client = http_client(&self.config)?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {} for {}", status, url));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = response
            .text()
            .await
            .map_err(|e| format!("Failed to read body: {}", e))?;
        let body = if raw || !content_type.contains("html") {
            text
        } else {
            html_to_text(&text)
        };
        if body.len() > 16_000 {
            return Ok(format!(
                "[Fetched {} chars from {}, truncated]\n{}\n\n[Use raw=true or a more specific URL for full content]",
                body.len(),
                url,
                truncate_utf8(&body, 8_000)
            ));
        }
        Ok(body)
    }
}

/// Search the web via a pluggable backend. The provider (and an optional
/// fallback) are selected from `[websearch]` config; see the [`search`] module
/// for the available backends. Default backend is Exa (hosted, anonymous,
/// reliable) with Parallel as fallback — mirroring other coding agents.
///
/// This struct is a thin shell: it only parses arguments, builds the shared
/// HTTP client (proxy/timeout), and delegates to the provider chain. All
/// backend-specific logic lives behind the `SearchProvider` trait so new
/// backends can be added without touching this tool.
pub struct WebSearchTool {
    config: Arc<WebSearchConfig>,
    primary: Box<dyn SearchProvider>,
    fallback: Option<Box<dyn SearchProvider>>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self::with_config(WebSearchConfig::default())
    }

    pub fn with_config(config: WebSearchConfig) -> Self {
        let primary = search::build_provider(&config, &config.provider);
        let fallback_name = config.fallback.trim();
        let fallback = if fallback_name.is_empty() {
            None
        } else {
            Some(search::build_provider(&config, fallback_name))
        };
        Self {
            config: Arc::new(config),
            primary,
            fallback,
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> &str {
        "Search the web and return results as text. The backend is configurable via the \
         `[websearch]` table in config.toml: `exa` (default; hosted, anonymous, reliable), \
         `parallel` (hosted), `duckduckgo` (keyless scraping, frequently blocked), `searxng` \
         (self-hosted, keyless), or `tavily` (hosted, needs key). A `fallback` backend is \
         tried automatically if the primary fails. Best for current information, \
         documentation, or examples."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query" }
            },
            "required": ["query"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let query = args["query"].as_str().ok_or("Missing 'query'")?;
        let client = http_client(&self.config)?;

        match self.primary.search(&client, query).await {
            Ok(text) => Ok(text),
            Err(primary_err) => match &self.fallback {
                Some(fallback) => match fallback.search(&client, query).await {
                    Ok(text) => Ok(text),
                    Err(fallback_err) => Err(format!(
                        "Primary backend {} failed: {}\nFallback backend {} also failed: {}",
                        self.primary.name(),
                        primary_err,
                        fallback.name(),
                        fallback_err
                    )),
                },
                None => Err(primary_err),
            },
        }
    }
}

/// A lightweight, standalone task list (decoupled from the persistent goal).
/// Useful as a scratchpad when no goal is active. State is in-process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

pub struct TodoWriteTool {
    items: Arc<Mutex<Vec<TodoItem>>>,
}

impl TodoWriteTool {
    pub fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        "Maintain a standalone task list (independent of the active goal). Replace the whole list \
         each call with the current set of concrete steps. Keep at most one item in_progress. \
         The returned list reflects the updated state."
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
                                "enum": ["pending", "in_progress", "completed"]
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
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Arguments {
            items: Vec<TodoArgs>,
        }
        #[derive(serde::Deserialize)]
        struct TodoArgs {
            content: String,
            status: String,
        }

        let parsed: Arguments =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        if parsed.items.len() > 50 {
            return Err("Todo list is limited to 50 items.".to_string());
        }
        let mut items = Vec::with_capacity(parsed.items.len());
        let mut in_progress = 0;
        for entry in parsed.items {
            if entry.content.trim().is_empty() {
                return Err("Todo item content cannot be empty.".to_string());
            }
            let status = match entry.status.as_str() {
                "pending" => TodoStatus::Pending,
                "in_progress" => {
                    in_progress += 1;
                    TodoStatus::InProgress
                }
                "completed" => TodoStatus::Completed,
                other => return Err(format!("Unknown todo status '{}'.", other)),
            };
            items.push(TodoItem {
                content: entry.content,
                status,
            });
        }
        if in_progress > 1 {
            return Err("At most one todo item may be in_progress.".to_string());
        }
        *self.items.lock().unwrap_or_else(|error| error.into_inner()) = items.clone();
        let rendered = render_todo(&items);
        Ok(format!("Todo list updated:\n{}", rendered))
    }
}

fn render_todo(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(empty)".to_string();
    }
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let mark = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
            };
            format!("{}. {} {}", idx + 1, mark, item.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn todo_tool_renders_updated_list() {
        let tool = TodoWriteTool::new();
        let output = tool
            .call(
                r#"{"items":[
                    {"content":"design","status":"completed"},
                    {"content":"implement","status":"in_progress"},
                    {"content":"verify","status":"pending"}
                ]}"#,
            )
            .await
            .unwrap();
        assert!(output.contains("[x] design"));
        assert!(output.contains("[~] implement"));
        assert!(output.contains("[ ] verify"));
        let snapshot = tool.snapshot();
        assert_eq!(snapshot.len(), 3);
    }

    #[tokio::test]
    async fn todo_tool_rejects_multiple_in_progress() {
        let tool = TodoWriteTool::new();
        let error = tool
            .call(
                r#"{"items":[
                    {"content":"a","status":"in_progress"},
                    {"content":"b","status":"in_progress"}
                ]}"#,
            )
            .await
            .unwrap_err();
        assert!(error.contains("At most one"));
    }

    #[test]
    fn html_to_text_handles_multibyte_before_script_tags() {
        let html = "人工<script>hidden</script>智能<style>.x{}</style>新闻";

        assert_eq!(html_to_text(html), "人工智能新闻");
    }

    #[test]
    fn truncate_utf8_does_not_split_multibyte_chars() {
        let text = "prefix ’ suffix";
        let inside_curly_quote = text.find('’').unwrap() + 1;

        assert_eq!(truncate_utf8(text, inside_curly_quote), "prefix ");
    }

    #[test]
    fn websearch_config_defaults_to_exa_with_parallel_fallback() {
        let cfg = WebSearchConfig::default();
        assert_eq!(cfg.provider, "exa");
        assert_eq!(cfg.fallback, "parallel");
        assert!(cfg.proxy.is_none());
        assert_eq!(cfg.timeout_secs, 20);
    }

    #[test]
    fn websearch_config_round_trips_through_toml() {
        let toml = r#"
            provider = "searxng"
            fallback = ""
            proxy = "socks5h://127.0.0.1:1080"
            timeout_secs = 8
            searxng_url = "http://localhost:8080/search"
        "#;
        let cfg: WebSearchConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.provider, "searxng");
        assert_eq!(cfg.fallback, "");
        assert_eq!(cfg.proxy.as_deref(), Some("socks5h://127.0.0.1:1080"));
        assert_eq!(cfg.timeout_secs, 8);
        assert_eq!(
            cfg.searxng_url.as_deref(),
            Some("http://localhost:8080/search")
        );
    }

    #[test]
    fn write_and_edit_tools_allow_plan_paths_in_plan_mode() {
        // The plans directory must exist so is_plan_path can resolve it.
        let cwd = std::env::current_dir().unwrap();
        std::fs::create_dir_all(cwd.join(neenee_core::plan::PLANS_DIR)).unwrap();

        let write = WriteFileTool;
        let edit = EditFileTool;

        let plan_args = r#"{"path":".neenee/plans/feature.md","content":"x"}"#;
        let plan_edit_args =
            r#"{"path":".neenee/plans/feature.md","old_string":"a","new_string":"b"}"#;
        let src_args = r#"{"path":"src/main.rs","content":"x"}"#;

        assert!(write.allowed_in_plan_mode(plan_args));
        assert!(edit.allowed_in_plan_mode(plan_edit_args));
        // Non-plan paths are not exempted, even though the tools are write-capable.
        assert!(!write.allowed_in_plan_mode(src_args));
        assert!(!edit.allowed_in_plan_mode(src_args));
    }
}
