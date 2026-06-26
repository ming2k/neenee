use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

/// Read a file from disk.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file. `path` is required. Each line is prefixed with its \
         line number. Supports `offset` (1-based start line) and `limit` (max \
         lines to read). Output is paginated (~50 KB per page); large reads \
         return the first chunk and indicate the next `offset` to continue.\n\
         \n\
         - Use `grep` first to find specific content in large files.\n\
         - To inspect multiple scattered lines, make a single read encompassing the entire range.\n\
         - Do not use this tool for directories (use `list_dir`) or binary files."
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
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let path = args["path"].as_str().ok_or("Missing 'path'")?;

        // Reject directories with an explicit, actionable message instead of
        // the raw OS "Is a directory (os error 21)". A model that sees the OS
        // error cannot infer it should switch to `list_dir`, and may re-read
        // the same directory in a loop. This mirrors the empty/EOF guidance
        // pattern: a clear reason breaks the loop.
        if std::path::Path::new(path).is_dir() {
            return Err(format!(
                "'{}' is a directory, not a file. Use the `list_dir` tool to see its contents.",
                path
            ));
        }

        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read '{}': {}", path, e))?;

        let lang = std::path::Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_string());

        if is_binary_extension(path) || is_binary_content(&bytes[..bytes.len().min(4096)]) {
            return Err(format!("Cannot read binary file: {}", path));
        }

        let content =
            String::from_utf8(bytes).map_err(|_| format!("File '{}' is not valid UTF-8", path))?;

        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(0) as usize;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

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
        // `offset` and can never get stuck re-truncating the same window.
        // The first line is always included even if it alone exceeds the
        // budget, so a read always makes forward progress.
        const READ_BUDGET_BYTES: usize = 50_000;
        const MAX_LINE_LENGTH: usize = 2000;
        const MAX_LINE_SUFFIX: &str = "... (line truncated)";
        let requested_end = if limit > 0 {
            (start + limit).min(total_lines)
        } else {
            total_lines
        };
        // Pre-compute truncated lines so cost reflects what we actually return.
        let truncated_lines: Vec<String> = lines[start..requested_end]
            .iter()
            .map(|line| {
                if line.len() > MAX_LINE_LENGTH {
                    let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
                    format!("{}{}", truncated, MAX_LINE_SUFFIX)
                } else {
                    line.to_string()
                }
            })
            .collect();
        let mut used = 0usize;
        let mut shown_end = start; // exclusive index into `lines`
        for (idx, truncated) in truncated_lines.iter().enumerate() {
            let i = start + idx;
            let cost = truncated.len() + 1; // +1 for the '\n' we rejoin with
            if i > start && used + cost > READ_BUDGET_BYTES {
                break;
            }
            used += cost;
            shown_end = i + 1;
        }

        let shown_count = shown_end - start;
        let text = truncated_lines[..shown_count].join("\n");
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
                    "[{} more line{} below — read {} with offset={}]",
                    remaining,
                    if remaining == 1 { "" } else { "s" },
                    path,
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
neenee_core::register_tool!(ReadFileFactory => ReadFileTool);

/// Extensions that are always treated as binary and never read as text.
const BINARY_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "o", "a", "lib",
    "class", "jar", "war", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "bin",
    "dat", "obj", "wasm", "pyc", "pyo", "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tiff",
    "tif", "mp3", "mp4", "avi", "mov", "mkv", "flv", "wav", "flac", "ogg", "pdf", "sqlite", "db",
    "mdb",
];

fn is_binary_extension(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_binary_content(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if bytes.contains(&0) {
        return true;
    }
    let non_printable = bytes
        .iter()
        .filter(|&&b| b < 9 || (b > 13 && b < 32))
        .count();
    non_printable as f64 / bytes.len() as f64 > 0.3
}
