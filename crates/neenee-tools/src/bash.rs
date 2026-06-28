use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

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
    fn scope_target(&self, arguments: &str) -> neenee_core::ScopeTarget {
        neenee_core::ScopeTarget::Command(json_string(arguments, "command"))
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<neenee_core::ToolOutput, String> {
        // Non-streaming path: delegate with no-op sinks.
        self.call_structured_with_events("", arguments, Box::new(|_| {}), &mut |_| {})
            .await
    }

    /// Spawn the command with piped stdout/stderr and merge both streams into a
    /// single, arrival-ordered line buffer so the renderer never has to choose
    /// the "all-stdout-then-all-stderr" split (which loses interleaving for
    /// tools like `cargo`/`git`/`npm`, whose progress/warnings hit stderr while
    /// results hit stdout). Both pipes are read on separate tasks and funnelled
    /// through one channel; the main future drains it in order, which is also
    /// where the `&mut` stream sink fires (it can't cross a spawned-task
    /// boundary).
    ///
    /// Each captured line is ANSI-stripped at the source: many commands emit
    /// colour even under a non-tty (`--color=always`, `CLICOLOR_FORCE`, a
    /// forced `.bashrc`), and raw `\x1b[...]m` bytes would corrupt the TUI's
    /// width math and read as literal `[0;32m` glyphs.
    async fn call_structured_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(neenee_core::EnvoyEvent) + Send + 'a>,
        on_stream: &mut (dyn FnMut(neenee_core::ToolStream) + Send + 'a),
    ) -> Result<neenee_core::ToolOutput, String> {
        use neenee_core::tool_output::{ShellLine, ShellStream, strip_ansi};
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

        // One merged channel: both pipes push (stream, line) here in arrival
        // order, so the drained `lines` preserves stdout/stderr interleaving.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(ShellStream, String)>();

        // Read stderr on a separate task so a full stderr pipe can't block the
        // child while stdout is being read. Each line is ANSI-stripped before
        // it enters the merged channel.
        let tx_err = tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_err.send((ShellStream::Err, strip_ansi(&line)));
            }
        });

        // Read stdout on its own task too, so both pipes drain concurrently and
        // their lines land in the channel in true arrival order.
        let tx_out = tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_out.send((ShellStream::Out, strip_ansi(&line)));
            }
        });

        // `kill_on_drop` guarantees the child is terminated when this future is
        // dropped — on timeout (the `Timeout` wrapper drops the inner future)
        // and on mid-run interrupt.
        let run = async {
            stdout_task.await.ok();
            stderr_task.await.ok();
            drop(tx); // close so the drain below terminates

            let status = child
                .wait()
                .await
                .map_err(|e| format!("Failed to wait: {}", e))?;
            let exit = status.code();

            // Drain the merged channel in arrival order. This is the only place
            // the `&mut` stream sink fires, so it sees the same interleaving as
            // the final `lines`. Rebuild the flat stdout/stderr strings the
            // model-facing path expects alongside the ordered view.
            let mut lines: Vec<ShellLine> = Vec::new();
            let mut stdout_buf = String::new();
            let mut stderr_buf = String::new();
            while let Some((stream, text)) = rx.recv().await {
                match stream {
                    ShellStream::Out => {
                        stdout_buf.push_str(&text);
                        stdout_buf.push('\n');
                        on_stream(neenee_core::ToolStream::Stdout(format!("{}\n", text)));
                    }
                    ShellStream::Err => {
                        stderr_buf.push_str(&text);
                        stderr_buf.push('\n');
                        on_stream(neenee_core::ToolStream::Stderr(format!("{}\n", text)));
                    }
                }
                lines.push(ShellLine { stream, text });
            }

            let truncated =
                neenee_core::tool_output::shell_inner_text(&stdout_buf, &stderr_buf, exit).len()
                    > 8000;
            Ok(neenee_core::ToolOutput::Shell {
                command: command.to_string(),
                stdout: stdout_buf,
                stderr: stderr_buf,
                lines,
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
