use async_trait::async_trait;
use neenee_core::{truncate_utf8, Tool, ToolAccess, WebSearchConfig};
use serde_json::json;
use std::sync::Arc;
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
        "Read the full contents of a file as text. Use when you need to see \
         code or text. Supports `offset` (1-based start line) and `limit` \
         (max lines). Output is paginated by whole lines with a byte budget, \
         so a large read returns the first chunk plus a concrete \
         `offset=<next>` to continue — always advance to that exact offset \
         and never re-read the same range. The result declares its line range \
         (`lines A-B of N`); an empty range means you are past EOF. Prefer \
         `offset`/`limit` over re-issuing the same full read."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or relative path to the file" },
                "offset": { "type": "integer", "description": "1-based line to start reading from (default 1)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read (default: to EOF / until the byte budget is hit)" }
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
        let total_lines = lines.len();
        let lang = std::path::Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_string());

        // Empty file / offset past EOF: nothing to show. Surface an explicit,
        // machine-actionable note so the model does NOT re-read in a loop
        // wondering whether the call failed. `text` stays empty (the renderer
        // draws nothing) and the note explains why via `to_text()`.
        let start = offset - 1;
        if total_lines == 0 {
            return Ok(neenee_core::ToolOutput::Code {
                lang,
                text: String::new(),
                start_line: offset,
                prefix: Some(format!("[{}: empty file]", path)),
                suffix: None,
            });
        }
        if start >= total_lines {
            return Ok(neenee_core::ToolOutput::Code {
                lang,
                text: String::new(),
                start_line: offset,
                prefix: Some(format!(
                    "[{}: offset {} is past end of file ({} line{})]",
                    path,
                    offset,
                    total_lines,
                    if total_lines == 1 { "" } else { "s" }
                )),
                suffix: None,
            });
        }

        // Requested window [start, requested_end): offset..offset+limit
        // (limit 0 = to EOF). We then snap this to a byte budget AT LINE
        // BOUNDARIES, which is what makes pagination deterministic and
        // loop-safe: every read returns whole lines plus a concrete
        // continuation offset, so the model can always compute the next
        // `offset` and can never get stuck re-truncating the same window
        // (the old char-mid-cut + "use offset/limit" with no number could).
        // The first line is always included even if it alone exceeds the
        // budget, so a read always makes forward progress.
        const READ_BUDGET_BYTES: usize = 8000;
        let requested_end = if limit > 0 {
            (start + limit).min(total_lines)
        } else {
            total_lines
        };
        let mut used = 0usize;
        let mut shown_end = start; // exclusive index into `lines`
        for i in start..requested_end {
            let cost = lines[i].len() + 1; // +1 for the '\n' we rejoin with
            if i > start && used + cost > READ_BUDGET_BYTES {
                break;
            }
            used += cost;
            shown_end = i + 1;
        }

        let text = lines[start..shown_end].join("\n");
        // 1-based range of what we actually returned.
        let first_line = offset; // == start + 1
        let last_line = shown_end; // exclusive 0-based index → 1-based last
        let more_remain = shown_end < total_lines;

        // Model-facing framing. Omitted entirely for the plain "read whole
        // small file from line 1" case (zero overhead, byte-identical to the
        // legacy model output); added whenever position or pagination matters
        // so the model always knows where it is and how to continue. The
        // renderer ignores prefix/suffix and gutter-numbers `text`.
        let (prefix, suffix) = if offset == 1 && !more_remain {
            (None, None)
        } else {
            let header = format!(
                "[{}: lines {}-{} of {}{}]",
                path,
                first_line,
                last_line,
                total_lines,
                if more_remain { "" } else { " (end of file)" }
            );
            let suffix = if more_remain {
                let remaining = total_lines - shown_end;
                Some(format!(
                    "[{} more line{} below — read with offset={}]",
                    remaining,
                    if remaining == 1 { "" } else { "s" },
                    shown_end + 1
                ))
            } else {
                None
            };
            (Some(header), suffix)
        };

        Ok(neenee_core::ToolOutput::Code {
            lang,
            text,
            start_line: offset,
            prefix,
            suffix,
        })
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
    /// it is the tier the `VERIFY` subagent profile admits so an independent
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
        _on_event: Box<dyn FnMut(neenee_core::SubagentEvent) + Send + 'a>,
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

// --- Self-registration -----------------------------------------------------
//
// Each tool registers itself here instead of being enumerated at the agent's
// assembly point. `inventory` collects these submissions at runtime; adding a
// tool is a single line in its own module (see project.rs for the scaffolding
// tools). The two web tools pull their config out of the build context.

neenee_core::register_tool!(BashFactory => BashTool);
neenee_core::register_tool!(ReadFileFactory => ReadFileTool);
neenee_core::register_tool!(WriteFileFactory => WriteFileTool);
neenee_core::register_tool!(AskUserFactory => AskUserTool);
neenee_core::register_tool!(EditFileFactory => EditFileTool);
neenee_core::register_tool!(GrepFactory => GrepTool);
neenee_core::register_tool!(GlobFactory => GlobTool);
neenee_core::register_tool!(ListDirFactory => ListDirTool);
neenee_core::register_tool!(WebFetchFactory => |ctx| {
    let cfg = ctx
        .get::<neenee_core::WebSearchConfig>()
        .cloned()
        .unwrap_or_default();
    WebFetchTool::with_config(cfg)
});
neenee_core::register_tool!(WebSearchFactory => |ctx| {
    let cfg = ctx
        .get::<neenee_core::WebSearchConfig>()
        .cloned()
        .unwrap_or_default();
    WebSearchTool::with_config(cfg)
});

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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Plan-mode path exemption was removed (ADR-0027/0028): scoped writes
        // are now expressed per-agent via `WriteScope`, not via an
        // `allowed_in_plan_mode` override on the write tools. This test is
        // kept as a placeholder guard that the write tools still build; the
        // scoping behavior is covered by neenee-core's WriteScope tests.
        let _write = WriteFileTool;
        let _edit = EditFileTool;
    }

    #[tokio::test]
    async fn read_file_carries_offset_as_start_line() {
        // The structured `Code::start_line` is the contract the renderer relies
        // on to number an offset snippet from its true file line. A read with
        // `offset: 3` must surface `start_line: 3` (and only the post-offset
        // content), while a plain read reports `start_line: 1`.
        let dir =
            std::env::temp_dir().join(format!("neenee-read-start-line-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lines.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

        let tool = ReadFileTool;

        let full = tool
            .call_structured(&r#"{"path":"PATH"}"#.replace("PATH", &path.to_string_lossy()))
            .await
            .unwrap();
        match full {
            neenee_core::ToolOutput::Code {
                start_line, text, ..
            } => {
                assert_eq!(start_line, 1);
                assert!(text.starts_with("one"));
            }
            _ => panic!("expected Code"),
        }

        let offset = tool
            .call_structured(
                &r#"{"path":"PATH","offset":3}"#.replace("PATH", &path.to_string_lossy()),
            )
            .await
            .unwrap();
        match offset {
            neenee_core::ToolOutput::Code {
                start_line, text, ..
            } => {
                assert_eq!(start_line, 3);
                assert_eq!(text, "three\nfour\nfive");
            }
            _ => panic!("expected Code"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Pull `(text, prefix, suffix)` out of a `Code` output for assertions.
    fn code_parts(
        out: neenee_core::ToolOutput,
    ) -> (String, Option<String>, Option<String>) {
        match out {
            neenee_core::ToolOutput::Code {
                text,
                prefix,
                suffix,
                ..
            } => (text, prefix, suffix),
            _ => panic!("expected Code output"),
        }
    }

    /// A file whose every line is exactly `line_width` chars so the byte-budget
    /// math is predictable in the pagination tests below.
    fn make_fixed_width_file(line_count: usize) -> (std::path::PathBuf, Vec<String>) {
        let dir = std::env::temp_dir()
            .join(format!("neenee-read-paginate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        let lines: Vec<String> = (1..=line_count).map(|n| format!("line{n:05}")).collect();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        (path, lines)
    }

    #[tokio::test]
    async fn plain_small_read_has_no_framing() {
        // The common case stays byte-identical to the legacy model output:
        // no prefix/suffix, so we don't tax every small read.
        let dir =
            std::env::temp_dir().join(format!("neenee-read-plain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let out = ReadFileTool
            .call_structured(&r#"{"path":"PATH"}"#.replace("PATH", &path.to_string_lossy()))
            .await
            .unwrap();
        let (text, prefix, suffix) = code_parts(out);
        assert_eq!(text, "a\nb\nc");
        assert!(prefix.is_none());
        assert!(suffix.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn large_read_paginates_with_concrete_non_overlapping_continuation() {
        // 3000 lines × 10 bytes ("lineNNNNN\n") = 30KB. The 8000-byte budget
        // holds ~800 lines per page. The tool MUST return whole lines, declare
        // the range, and give an exact next offset — and following that offset
        // must continue without overlap or gap (the loop-safety contract).
        const LINES: usize = 3000;
        const PAGE: usize = 800; // 8000 / (9 + 1)
        let (path, lines) = make_fixed_width_file(LINES);
        let tool = ReadFileTool;
        let arg = |offset: usize| {
            format!(
                r#"{{"path":"{}","offset":{}}}"#,
                path.to_string_lossy(),
                offset
            )
        };

        // Page 1: lines 1..=800, continuation offset = 801.
        let (text1, pre1, suf1) = code_parts(tool.call_structured(&arg(1)).await.unwrap());
        assert_eq!(
            pre1,
            Some(format!(
                "[{}: lines 1-{} of {}]",
                path.to_string_lossy(),
                PAGE,
                LINES
            ))
        );
        let suf1 = suf1.expect("page 1 has a continuation suffix");
        assert!(
            suf1.contains("offset=801"),
            "suffix must name the exact next offset, got: {suf1}"
        );
        assert_eq!(text1.lines().count(), PAGE);
        assert_eq!(text1.lines().next().unwrap(), "line00001");
        assert_eq!(text1.lines().last().unwrap(), &format!("line{:05}", PAGE));

        // Page 2 from the advertised offset: must start exactly at 801 (no gap)
        // and not repeat line 800 (no overlap) — this is what breaks the loop.
        let (text2, _pre2, suf2) = code_parts(tool.call_structured(&arg(801)).await.unwrap());
        assert_eq!(text2.lines().next().unwrap(), "line00801", "no gap");
        assert!(
            !text2.lines().any(|l| l == "line00800"),
            "no overlap with previous page"
        );
        assert_eq!(text2.lines().count(), PAGE);
        assert!(
            suf2.expect("page 2 suffix").contains("offset=1601"),
            "continuation advances"
        );

        // Final page reaches EOF and carries no continuation suffix.
        let (text_last, _pre, suf_last) =
            code_parts(tool.call_structured(&arg(LINES - PAGE + 1)).await.unwrap());
        assert_eq!(
            text_last.lines().last().unwrap(),
            &format!("line{:05}", LINES),
            "lands exactly on the last line"
        );
        assert!(suf_last.is_none(), "no suffix at EOF");

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn oversized_limit_is_line_bounded_not_re_truncated() {
        // Regression for the real infinite-loop trap: requesting a huge `limit`
        // on a big file used to keep the slice over budget, re-truncate the
        // same window, and emit a generic "use offset/limit" with no number.
        // Now the window is line-bounded and the continuation is concrete, so
        // the model advances instead of looping.
        const LINES: usize = 3000;
        let (path, _lines) = make_fixed_width_file(LINES);
        let arg = format!(
            r#"{{"path":"{}","limit":{}}}"#,
            path.to_string_lossy(),
            LINES
        );
        let (text, _pre, suf) = code_parts(ReadFileTool.call_structured(&arg).await.unwrap());
        // Far fewer than the requested 3000 lines — bounded by the budget.
        assert!(text.lines().count() < LINES);
        assert!(
            suf.expect("oversized limit still paginates").contains("offset="),
            "gives a concrete next offset rather than a generic hint"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn empty_and_past_eof_reads_explain_themselves() {
        // Both cases used to return a bare empty string, which a model can
        // mistake for a failure and re-read in a loop. They now carry an
        // explicit note via the model-facing prefix.
        let dir = std::env::temp_dir()
            .join(format!("neenee-read-edge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "").unwrap();
        let (text, pre, suf) = code_parts(
            ReadFileTool
                .call_structured(&r#"{"path":"PATH"}"#.replace("PATH", &empty.to_string_lossy()))
                .await
                .unwrap(),
        );
        assert!(text.is_empty());
        assert!(
            pre.as_ref().is_some_and(|p| p.contains("empty file")),
            "pre={pre:?}"
        );
        assert!(suf.is_none());

        let small = dir.join("small.txt");
        std::fs::write(&small, "a\nb\n").unwrap();
        let (text, pre, suf) = code_parts(
            ReadFileTool
                .call_structured(
                    &r#"{"path":"PATH","offset":99}"#.replace("PATH", &small.to_string_lossy()),
                )
                .await
                .unwrap(),
        );
        assert!(text.is_empty());
        assert!(
            pre.as_ref().is_some_and(|p| p.contains("past end of file")),
            "pre={pre:?}"
        );
        assert!(suf.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
