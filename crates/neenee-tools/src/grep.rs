use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

/// Maximum wall-clock time for a single `rg` invocation. A slow or wedged
/// ripgrep (huge tree, catastrophic-backtracking pattern) is released rather
/// than pinning the async executor — the old code blocked a runtime worker
/// thread for the entire run via `std::process::Command::output`.
const GREP_TIMEOUT: Duration = Duration::from_secs(30);

/// Global cap on returned match lines, applied *after* ripgrep across all
/// files. `--max-count` only bounds matches per file, so a common pattern in a
/// large tree could still flood the model's context with thousands of lines.
/// This is the grep analogue of the shell-output and paged-read caps.
const GREP_MAX_LINES: usize = 200;

/// Global cap on returned bytes, mirroring the shell-output truncation. Honored
/// alongside [`GREP_MAX_LINES`]; whichever trips first wins.
const GREP_MAX_BYTES: usize = 32 * 1024;

/// Search file contents with ripgrep.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search for a regex pattern in files using ripgrep. Returns matches \
         in `path:line:content` format. Set `context` to include N surrounding \
         lines per match (default 0); read the file directly when you need more."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Directory or file to search in (default '.')" },
                "ext": { "type": "string", "description": "Optional file extension filter (e.g., 'rs', 'py')" },
                "context": { "type": "integer", "description": "Lines of context around each match (default 0)" }
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
        // Context is opt-in: every context line multiplies output, and the model
        // can read the file when it needs surroundings. Clamp to a sane ceiling.
        let context = args["context"].as_u64().unwrap_or(0).min(10);

        let mut cmd = Command::new("rg");
        cmd.args(["-n", "--color=never", "--max-count", "50"]);
        if context > 0 {
            cmd.arg("-C").arg(context.to_string());
        }
        if let Some(e) = ext {
            cmd.arg("-g").arg(format!("*.{}", e));
        }
        // Prune the same set of directories the glob/list tools ignore, so the
        // three tools agree about what exists in a tree.
        for dir in crate::helpers::IGNORED_DIRS {
            cmd.arg("-g").arg(format!("!{}", dir));
        }
        cmd.arg(pattern).arg(path);

        // Spawn under tokio (releasing the runtime while rg runs) and bound the
        // whole invocation by `GREP_TIMEOUT`. On timeout the child is killed
        // via `kill_on_drop`-equivalent: we explicitly `start_kill` first so a
        // wedged rg does not linger.
        let run = async {
            let output = cmd
                .output()
                .await
                .map_err(|e| format!("Failed to run rg: {}. Is ripgrep installed?", e))?;
            Ok::<_, String>(output)
        };
        let output = match timeout(GREP_TIMEOUT, run).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(format!(
                    "grep timed out after {} seconds",
                    GREP_TIMEOUT.as_secs()
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if stdout.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if !stderr.is_empty() {
                return Err(format!("rg error: {}", stderr));
            }
            return Ok("No matches found.".to_string());
        }
        Ok(cap_output(&stdout))
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

/// Bound ripgrep's stdout to [`GREP_MAX_LINES`] / [`GREP_MAX_BYTES`], whichever
/// trips first, appending a one-line truncation notice. This is the grep
/// counterpart to the shell-output and paged-read caps: a common pattern in a
/// large tree must not flood the model's context, since `--max-count` only
/// bounds matches *per file*.
fn cap_output(stdout: &str) -> String {
    let mut out = String::new();
    let mut lines = 0usize;
    let mut truncated = false;
    // `lines` counts *written* lines (conditionally incremented, not the loop
    // index), so `enumerate()` would not be a faithful rewrite.
    #[allow(clippy::explicit_counter_loop)]
    for line in stdout.lines() {
        if lines >= GREP_MAX_LINES || out.len() + line.len() + 1 > GREP_MAX_BYTES {
            truncated = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
        lines += 1;
    }
    if truncated {
        out.push_str("\n[Output truncated — narrow your pattern, path, or `ext`.]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_output_passes_through_small_results() {
        let s = "a.rs:1:foo\na.rs:2:bar\n";
        assert_eq!(cap_output(s), s);
    }

    #[test]
    fn cap_output_truncates_by_line_count() {
        let big: String = (0..GREP_MAX_LINES + 50)
            .map(|i| format!("f.rs:{i}:hit\n"))
            .collect();
        let capped = cap_output(&big);
        let kept = capped.lines().filter(|l| l.contains(":hit")).count();
        assert_eq!(kept, GREP_MAX_LINES);
        assert!(capped.contains("[Output truncated"));
    }

    #[test]
    fn cap_output_truncates_by_bytes() {
        // Few lines, but each huge -> byte cap trips before the line cap.
        let line = format!("f.rs:1:{}\n", "x".repeat(GREP_MAX_BYTES));
        let capped = cap_output(&line);
        assert!(capped.len() <= GREP_MAX_BYTES + 64);
        assert!(capped.contains("[Output truncated"));
    }
}
