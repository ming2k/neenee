use async_trait::async_trait;
use neenee_core::{Tool, ToolAccess};
use serde_json::json;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::helpers::json_string;

/// Execute a bash command.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    /// `bash` runs commands — its primary purpose is execution, not workspace
    /// mutation — so it sits in the `Execute` tier between pure reads and
    /// file-writing tools. The broker still gates it (`Execute > Read`). See
    /// ADR-0012.
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

neenee_core::register_tool!(BashFactory => BashTool);
