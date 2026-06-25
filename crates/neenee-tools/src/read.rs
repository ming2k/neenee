use async_trait::async_trait;
use neenee_core::{Tool, ToolAccess};
use serde_json::json;

/// Read a file from disk.
pub struct ReadFileTool;

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
neenee_core::register_tool!(ReadFileFactory => ReadFileTool);
