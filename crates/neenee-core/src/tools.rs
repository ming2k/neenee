use crate::{Tool, ToolAccess};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

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
                truncate_utf8(&result, 4000)
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
    fn allowed_in_plan_mode(&self, arguments: &str) -> bool {
        crate::plan::is_plan_path(&json_string(arguments, "path"))
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
    fn allowed_in_plan_mode(&self, arguments: &str) -> bool {
        crate::plan::is_plan_path(&json_string(arguments, "path"))
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
        let timeout_secs = args["timeout"].as_u64().unwrap_or(30);
        let timeout_duration = Duration::from_secs(timeout_secs);

        let future = if cfg!(target_os = "windows") {
            Command::new("cmd").args(["/C", command]).output()
        } else {
            Command::new("sh").arg("-c").arg(command).output()
        };

        let output = timeout(timeout_duration, future)
            .await
            .map_err(|_| format!("Command timed out after {} seconds", timeout_secs))?
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
                truncate_utf8(&result, 4000)
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

        let mut cmd = std::process::Command::new("rg");
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
        ToolAccess::ReadOnly
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
pub struct WebFetchTool;

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("neenee/0.1 (+ai-coding-agent)")
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Naive HTML → text conversion. Collapses whitespace and strips tags/scripts.
fn html_to_text(html: &str) -> String {
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
        ToolAccess::ReadOnly
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let url = args["url"].as_str().ok_or("Missing 'url'")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("URL must start with http:// or https://".to_string());
        }
        let raw = args["raw"].as_bool().unwrap_or(false);
        let client = http_client()?;
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

/// Search the web using DuckDuckGo (no API key required, best-effort).
pub struct WebSearchTool;

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse DuckDuckGo HTML results. Tolerant to markup variations.
fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    // DDG html endpoint wraps result links in <a class="result__a" href="...">
    for piece in html.split("result__a") {
        if results.len() >= 10 {
            break;
        }
        let Some(href_start) = piece.find("href=\"") else {
            continue;
        };
        let rest = &piece[href_start + 6..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let raw_url = &rest[..end];
        // DDG redirects through /l/?uddg=<encoded>; extract the real url
        let url = decode_ddg_redirect(raw_url);
        if url.is_empty() || !url.starts_with("http") {
            continue;
        }
        let title_rest = &rest[end..];
        let title = extract_until(title_start_after(title_rest), '<');
        let snippet = extract_snippet(piece);
        if title.trim().is_empty() {
            continue;
        }
        results.push(SearchResult {
            title: decode_entities(&title),
            url,
            snippet: decode_entities(&snippet),
        });
    }
    results
}

/// Parse DuckDuckGo Lite results.
/// Lite uses `<a class="result-link" href="...">title</a>` and
/// `<td class="result-snippet">snippet</td>`.
fn parse_ddg_lite_results(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    // Split on the result link marker so each piece contains one result.
    for piece in html.split("result-link") {
        if results.len() >= 10 {
            break;
        }
        let Some(href_start) = piece.find("href=\"") else {
            continue;
        };
        let rest = &piece[href_start + 6..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let raw_url = &rest[..end];
        let url = decode_ddg_redirect(raw_url);
        if url.is_empty() || !url.starts_with("http") {
            continue;
        }
        let title = extract_until(title_start_after(&rest[end..]), '<');
        if title.trim().is_empty() {
            continue;
        }
        let snippet = extract_lite_snippet(piece);
        results.push(SearchResult {
            title: decode_entities(&title),
            url,
            snippet: decode_entities(&snippet),
        });
    }
    results
}

fn title_start_after(rest: &str) -> &str {
    if let Some(idx) = rest.find('>') {
        &rest[idx + 1..]
    } else {
        ""
    }
}

fn extract_until(text: &str, terminator: char) -> String {
    text.find(terminator)
        .map(|idx| text[..idx].to_string())
        .unwrap_or_else(|| text.to_string())
}

fn extract_snippet(piece: &str) -> String {
    if let Some(idx) = piece.find("result__snippet") {
        let rest = &piece[idx..];
        if let Some(gt) = rest.find('>') {
            return extract_until(&rest[gt + 1..], '<');
        }
    }
    String::new()
}

fn extract_lite_snippet(piece: &str) -> String {
    if let Some(idx) = piece.find("result-snippet") {
        let rest = &piece[idx..];
        if let Some(gt) = rest.find('>') {
            return extract_until(&rest[gt + 1..], '<');
        }
    }
    String::new()
}

fn decode_ddg_redirect(raw: &str) -> String {
    if let Some(stripped) = raw.strip_prefix("//duckduckgo.com/l/?uddg=") {
        let encoded = stripped.split('&').next().unwrap_or("");
        if let Ok(decoded) = url_decode(encoded) {
            return decoded;
        }
    }
    raw.to_string()
}

fn url_decode(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|e| e.to_string())?;
                let byte = u8::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                out.push(byte as char);
                i += 2;
            }
            c => out.push(c as char),
        }
        i += 1;
    }
    Ok(out)
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> &str {
        "Search the web and return the top results (title, url, snippet). Uses DuckDuckGo so no \
         API key is required. Best for finding current information, documentation, or examples."
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
        ToolAccess::ReadOnly
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let query = args["query"].as_str().ok_or("Missing 'query'")?;
        let client = http_client()?;

        // Try DuckDuckGo Lite first (simpler, more stable markup), then fall back to the
        // regular HTML endpoint. If both fail, surface the last error so the user/model can
        // tell what happened instead of getting a misleading "No results found".
        let (results, source) = match search_ddg_lite(&client, query).await {
            Ok(results) if !results.is_empty() => (results, "DuckDuckGo Lite"),
            _ => match search_ddg_html(&client, query).await {
                Ok(results) => (results, "DuckDuckGo HTML"),
                Err(error) => return Err(error),
            },
        };

        if results.is_empty() {
            return Ok(format!(
                "No results found for '{}' (tried DuckDuckGo Lite and HTML endpoints).",
                query
            ));
        }
        let formatted = results
            .iter()
            .enumerate()
            .map(|(idx, result)| {
                format!(
                    "{}. {}\n   {}\n   {}",
                    idx + 1,
                    result.title,
                    result.url,
                    result.snippet
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(format!(
            "Search results for '{}' (via {}):\n\n{}",
            query, source, formatted
        ))
    }
}

async fn search_ddg_lite(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let endpoint = "https://lite.duckduckgo.com/lite/";
    let response = client
        .post(endpoint)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .form(&[("q", query), ("kl", "us-en")])
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo Lite request failed: {}", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("DuckDuckGo Lite returned HTTP {}", status));
    }
    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read DuckDuckGo Lite response: {}", e))?;
    Ok(parse_ddg_lite_results(&html))
}

async fn search_ddg_html(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let endpoint = "https://html.duckduckgo.com/html/";
    let response = client
        .post(endpoint)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .form(&[("q", query), ("kl", "us-en")])
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo HTML request failed: {}", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("DuckDuckGo HTML returned HTTP {}", status));
    }
    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read DuckDuckGo HTML response: {}", e))?;
    Ok(parse_ddg_results(&html))
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
        ToolAccess::ReadOnly
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

/// Spawn a read-only exploration sub-agent to handle a research sub-task.
///
/// The sub-agent runs the same provider with the read-only subset of tools,
/// so it never prompts for permission and cannot mutate the workspace. Its
/// final answer is returned to the calling agent, which stays in control of
/// any write operations. Recursion is prevented by excluding `task` (and
/// other dispatch tools) from the sub-agent's toolset.
pub struct TaskTool {
    provider: Arc<dyn crate::Provider>,
    tools: Vec<Arc<dyn crate::Tool>>,
}

impl TaskTool {
    /// `tools` should be the parent agent's full toolset; the task tool filters
    /// it down to read-only tools for the spawned sub-agent.
    pub fn new(provider: Arc<dyn crate::Provider>, tools: Vec<Arc<dyn crate::Tool>>) -> Self {
        Self { provider, tools }
    }
}

const TASK_MAX_ROUNDS_HINT: &str = "Run at most a handful of tool rounds, then answer.";

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Launch a focused, read-only sub-agent to research or explore part of the codebase (or the \
         web) and return a concise written answer. Use it to parallelize investigation: finding \
         where code lives, summarizing files, gathering context. The sub-agent cannot modify \
         files — you perform any edits after reviewing its findings."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "Short label for the sub-task (<=60 chars)" },
                "prompt": { "type": "string", "description": "The full, self-contained instructions for the sub-agent" }
            },
            "required": ["description", "prompt"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.run_sub_agent(arguments, Box::new(|_| {})).await
    }

    async fn call_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(crate::SubTaskEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.run_sub_agent(arguments, on_event).await
    }
}

impl TaskTool {
    async fn run_sub_agent<'a>(
        &self,
        arguments: &str,
        mut on_event: Box<dyn FnMut(crate::SubTaskEvent) + Send + 'a>,
    ) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let description = args["description"]
            .as_str()
            .ok_or("Missing 'description'")?
            .trim();
        let prompt = args["prompt"].as_str().ok_or("Missing 'prompt'")?;
        if description.is_empty() {
            return Err("'description' must not be empty.".to_string());
        }
        if prompt.trim().is_empty() {
            return Err("'prompt' must not be empty.".to_string());
        }

        // Sub-agent gets read-only tools only; never itself (no recursion).
        let sub_tools: Vec<Arc<dyn crate::Tool>> = self
            .tools
            .iter()
            .filter(|tool| tool.access() == crate::ToolAccess::ReadOnly && tool.name() != "task")
            .cloned()
            .collect();

        let goal_service = crate::GoalService::new(
            crate::GoalStore::open_in_memory()
                .await
                .map_err(|err| format!("failed to create sub-agent goal store: {err}"))?,
        );
        let sub_agent =
            crate::Agent::new(self.provider.clone(), sub_tools, crate::AgentMode::Build, goal_service);

        let system = format!(
            "You are a focused research sub-agent. Your single job is to answer the assigned task \
             accurately and concisely using read-only tools. Explore the workspace or the web as \
             needed, then write a clear, complete final answer with the key findings (file paths, \
             signatures, relevant snippets, conclusions). Do not modify any files. {}\n\nTask: {}",
            TASK_MAX_ROUNDS_HINT, description,
        );
        let mut messages = vec![
            crate::Message::new(crate::Role::System, system),
            crate::Message::new(crate::Role::User, prompt.to_string()),
        ];
        let result = sub_agent
            .run_streaming_with_events(&mut messages, |event| {
                Self::forward_event(event, &mut on_event)
            })
            .await
            .map_err(|error| error.to_string())?;
        let content = result.message.content.trim().to_string();
        if content.is_empty() {
            Ok("(sub-agent returned no answer)".to_string())
        } else {
            Ok(content)
        }
    }

    fn forward_event(event: crate::AgentEvent, on_event: &mut dyn FnMut(crate::SubTaskEvent)) {
        match event {
            crate::AgentEvent::ModelRequestStarted { tool_round } => {
                let status = if tool_round == 0 {
                    "waiting for model".to_string()
                } else {
                    format!("waiting for model · round {}", tool_round + 1)
                };
                on_event(crate::SubTaskEvent::Activity(status));
            }
            crate::AgentEvent::AssistantDelta { delta, start } => {
                if start {
                    on_event(crate::SubTaskEvent::StreamStart);
                }
                on_event(crate::SubTaskEvent::StreamDelta(delta));
            }
            crate::AgentEvent::AssistantEnd(content) => {
                on_event(crate::SubTaskEvent::StreamEnd(content));
            }
            crate::AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                on_event(crate::SubTaskEvent::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            crate::AgentEvent::ToolResult {
                id,
                name,
                output,
                duration_ms,
            } => {
                on_event(crate::SubTaskEvent::ToolResult {
                    id,
                    name,
                    output,
                    duration_ms,
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Provider, Role};
    use futures::stream::{self, BoxStream};

    struct CannedProvider;

    #[async_trait::async_trait]
    impl Provider for CannedProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Ok(Message::new(Role::Assistant, "found 3 relevant files"))
        }
        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::once(async {
                Ok("found 3 relevant files".to_string())
            })))
        }
    }

    struct EchoReadTool;

    #[async_trait::async_trait]
    impl Tool for EchoReadTool {
        fn name(&self) -> &str {
            "echo_read"
        }
        fn description(&self) -> &str {
            "test read tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn access(&self) -> ToolAccess {
            ToolAccess::ReadOnly
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("echo".to_string())
        }
    }

    #[tokio::test]
    async fn task_tool_runs_read_only_subagent_and_returns_answer() {
        let tool = TaskTool::new(
            std::sync::Arc::new(CannedProvider),
            vec![std::sync::Arc::new(EchoReadTool)],
        );

        let output = tool
            .call(r#"{"description":"find files","prompt":"where are the handlers?"}"#)
            .await
            .unwrap();

        assert_eq!(output, "found 3 relevant files");
    }

    #[tokio::test]
    async fn task_tool_rejects_missing_fields() {
        let tool = TaskTool::new(std::sync::Arc::new(CannedProvider), Vec::new());
        assert!(tool.call(r#"{"description":"x"}"#).await.is_err());
        assert!(tool.call(r#"{"prompt":"x"}"#).await.is_err());
    }

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
    fn parse_ddg_results_extracts_title_url_and_snippet() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fnews">AI News</a>
            <a class="result__snippet">Latest artificial intelligence headlines.</a>
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org">Research</a>
            <a class="result__snippet">Research papers on AI.</a>
        "#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "AI News");
        assert_eq!(results[0].url, "https://example.com/news");
        assert_eq!(
            results[0].snippet,
            "Latest artificial intelligence headlines."
        );
    }

    #[test]
    fn parse_ddg_lite_results_extracts_title_url_and_snippet() {
        let html = r#"
            <a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Flite.example.com%2Fone">Lite Result One</a>
            <td class="result-snippet">A snippet from the lite endpoint.</td>
            <a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Flite.example.com%2Ftwo">Lite Result Two</a>
            <td class="result-snippet">Another lite snippet.</td>
        "#;
        let results = parse_ddg_lite_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Lite Result One");
        assert_eq!(results[0].url, "https://lite.example.com/one");
        assert_eq!(results[0].snippet, "A snippet from the lite endpoint.");
    }

    #[test]
    fn parse_ddg_results_skips_invalid_redirects() {
        let html = r#"
            <a class="result__a" href="/not-a-redirect">Bad Link</a>
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fvalid.example.com">Good Link</a>
        "#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Good Link");
    }

    #[test]
    fn write_and_edit_tools_allow_plan_paths_in_plan_mode() {
        // The plans directory must exist so is_plan_path can resolve it.
        let cwd = std::env::current_dir().unwrap();
        std::fs::create_dir_all(cwd.join(crate::plan::PLANS_DIR)).unwrap();

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
