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
        "Execute a shell command. Use for git, build, test, or any system operation. \
         A command that produces no output for 10 seconds is treated as blocked \
         (e.g. waiting on stdin) and is killed early even if `timeout` is longer; \
         long but healthy commands keep producing output and are not affected."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout": { "type": "integer", "description": "Overall timeout in seconds (default 30). A command producing no output for 10s is still killed early as a blocked-command guard." }
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
        // Non-streaming path: delegate with no-op sinks and the default
        // (closed) stdin policy. The streaming entry point below is where the
        // real stdin policy is applied.
        self.call_structured_with_events(
            "",
            arguments,
            Box::new(|_| {}),
            &mut |_| {},
            neenee_core::StdinPolicy::default(),
        )
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
        stdin_policy: neenee_core::StdinPolicy,
    ) -> Result<neenee_core::ToolOutput, String> {
        use neenee_core::tool_output::{
            ShellLine, ShellStream, normalize_carriage_returns, strip_ansi,
        };
        use tokio::io::{AsyncBufReadExt, BufReader};

        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let command = args["command"].as_str().ok_or("Missing 'command'")?;
        let timeout_secs = args["timeout"].as_u64().unwrap_or(30);
        let timeout_duration = Duration::from_secs(timeout_secs);

        // Resolve the stdin policy into the `Stdio` the child is spawned with.
        // `Closed` → `/dev/null` (the default hard floor: a child blocking on
        // `read(stdin)` gets instant EOF). `Prefilled` → a pipe we write the
        // bytes into right after spawn; the pipe buffer holds them ahead of
        // the child's first read. (L1 — see disclosure/bash design doc.)
        let stdin_bytes = match &stdin_policy {
            neenee_core::StdinPolicy::Closed => None,
            neenee_core::StdinPolicy::Prefilled { data } => Some(data.clone()),
        };
        let stdin_stdio = if stdin_bytes.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        };

        let mut child = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(["/C", command])
                .kill_on_drop(true)
                .stdin(stdin_stdio)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        } else {
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .kill_on_drop(true)
                .stdin(stdin_stdio)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        }
        .map_err(|e| format!("Failed to execute: {}", e))?;

        // For a prefilled stdin, write the bytes into the pipe and drop our
        // handle so the child sees EOF once it has consumed them. The pipe
        // buffer (≥ 4 KiB) holds a typical passphrase ahead of the child's
        // first read, so ordering relative to stdout is irrelevant.
        if let Some(bytes) = stdin_bytes
            && let Some(mut child_stdin) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            let _ = child_stdin.write_all(bytes.as_bytes()).await;
            let _ = child_stdin.shutdown().await;
        }

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
        // child while stdout is being read. Each line is ANSI-stripped and
        // carriage-return/backspace normalized (so a `\r`-refreshed progress
        // bar collapses to its final frame instead of being dropped or
        // mis-rendered) before it enters the merged channel.
        let tx_err = tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_err.send((
                    ShellStream::Err,
                    normalize_carriage_returns(&strip_ansi(&line)),
                ));
            }
        });

        // Read stdout on its own task too, so both pipes drain concurrently and
        // their lines land in the channel in true arrival order.
        let tx_out = tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx_out.send((
                    ShellStream::Out,
                    normalize_carriage_returns(&strip_ansi(&line)),
                ));
            }
        });

        // `kill_on_drop` guarantees the child is terminated when this future is
        // dropped — on timeout (the `Timeout` wrapper drops the inner future)
        // and on mid-run interrupt.
        //
        // L2 idle watchdog: the drain races each `recv()` against an idle
        // deadline. A command that produces zero output for longer than the
        // idle budget is almost certainly blocked waiting for stdin (a prompt
        // the agent cannot answer); killing it then — instead of burning the
        // entire wall-clock timeout — surfaces the failure fast and tags it
        // `IdleBlocked` so the footer can suggest a non-interactive retry.
        // Healthy long-running commands (build/test) keep producing lines,
        // resetting the deadline each time, so the idle timer never fires on
        // legitimate work.
        let idle_budget = Duration::from_secs(10);
        let run = async {
            stdout_task.await.ok();
            stderr_task.await.ok();
            drop(tx); // close so the drain below terminates

            // Drain the merged channel in arrival order, racing each recv
            // against the idle deadline. This is the only place the
            // `&mut` stream sink fires, so it sees the same interleaving as
            // the final `lines`. Rebuild the flat stdout/stderr strings the
            // model-facing path expects alongside the ordered view.
            let mut lines: Vec<ShellLine> = Vec::new();
            let mut stdout_buf = String::new();
            let mut stderr_buf = String::new();
            let mut idle_blocked = false;
            loop {
                // Reset the idle deadline each iteration: any output in the
                // last `idle_budget` keeps the command alive.
                let idle = tokio::time::sleep(idle_budget);
                tokio::pin!(idle);
                tokio::select! {
                    biased;
                    _ = &mut idle => {
                        // No output for the whole budget → assume stdin-blocked.
                        idle_blocked = true;
                        break;
                    }
                    msg = rx.recv() => {
                        match msg {
                            Some((stream, text)) => {
                                match stream {
                                    ShellStream::Out => {
                                        stdout_buf.push_str(&text);
                                        stdout_buf.push('\n');
                                        on_stream(neenee_core::ToolStream::Stdout(
                                            format!("{}\n", text),
                                        ));
                                    }
                                    ShellStream::Err => {
                                        stderr_buf.push_str(&text);
                                        stderr_buf.push('\n');
                                        on_stream(neenee_core::ToolStream::Stderr(
                                            format!("{}\n", text),
                                        ));
                                    }
                                }
                                lines.push(ShellLine { stream, text });
                            }
                            None => break, // channel closed → normal completion
                        }
                    }
                }
            }

            // If we broke out on the idle deadline, the child is still alive;
            // reap it (kill_on_drop would too, but reaping gives a real exit).
            // A blocked child may not have exited, so don't block on wait()
            // indefinitely — best-effort.
            let exit = if idle_blocked {
                let _ = child.start_kill();
                child.wait().await.ok().and_then(|s| s.code())
            } else {
                child.wait().await.ok().and_then(|s| s.code())
            };

            let termination = if idle_blocked {
                neenee_core::tool_output::ShellTermination::IdleBlocked
            } else {
                neenee_core::tool_output::ShellTermination::Exited
            };
            let truncated =
                neenee_core::tool_output::shell_inner_text(&stdout_buf, &stderr_buf, exit).len()
                    > neenee_core::tool_output::SHELL_MAX_OUTPUT_CHARS;
            Ok(neenee_core::ToolOutput::Shell {
                command: command.to_string(),
                stdout: stdout_buf,
                stderr: stderr_buf,
                lines,
                exit,
                truncated,
                termination,
            }) as Result<neenee_core::ToolOutput, String>
        };

        timeout(timeout_duration, run)
            .await
            .map_err(|_| format!("Command timed out after {} seconds", timeout_secs))?
    }
}

neenee_core::register_tool!(BashFactory => BashTool);

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy command captures stdout and exits cleanly with `Exited`.
    #[tokio::test]
    async fn bash_captures_stdout_and_exits() {
        let tool = BashTool;
        let out = tool
            .call_structured(r#"{"command":"printf hello"}"#)
            .await
            .expect("ok");
        match out {
            neenee_core::ToolOutput::Shell {
                stdout,
                exit,
                termination,
                ..
            } => {
                assert_eq!(stdout, "hello\n");
                assert_eq!(exit, Some(0));
                assert_eq!(
                    termination,
                    neenee_core::tool_output::ShellTermination::Exited
                );
            }
            other => panic!("expected Shell, got {:?}", other),
        }
    }

    /// The default stdin policy is Closed (`/dev/null`), so a command that
    /// reads stdin gets instant EOF and fails fast instead of hanging. This
    /// is the L1 hard floor: `cat` with no input and closed stdin exits 0
    /// immediately.
    #[tokio::test]
    async fn bash_closed_stdin_means_eof_not_hang() {
        let tool = BashTool;
        // `read line` under `sh -c` with stdin=/dev/null returns non-zero
        // immediately (EOF) rather than blocking.
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            tool.call_structured(r#"{"command":"read x"}"#),
        )
        .await
        .expect("closed stdin must NOT hang past 5s");
        match out.expect("ok") {
            neenee_core::ToolOutput::Shell { exit, .. } => {
                // `read` hits EOF → non-zero exit, but crucially it returned
                // at all (no hang).
                assert_ne!(exit, Some(0));
            }
            other => panic!("expected Shell, got {:?}", other),
        }
    }

    /// A prefilled stdin policy pipes the bytes into the child: `cat` echoes
    /// them back. This is the L3.5 seam (human/model input injection).
    #[tokio::test]
    async fn bash_prefilled_stdin_feeds_the_child() {
        let tool = BashTool;
        let mut on_stream = |_: neenee_core::ToolStream| ();
        let out = tool
            .call_structured_with_events(
                "",
                r#"{"command":"cat"}"#,
                Box::new(|_| {}),
                &mut on_stream,
                neenee_core::StdinPolicy::Prefilled {
                    data: "injected\n".into(),
                },
            )
            .await
            .expect("ok");
        match out {
            neenee_core::ToolOutput::Shell { stdout, exit, .. } => {
                assert_eq!(stdout, "injected\n");
                assert_eq!(exit, Some(0));
            }
            other => panic!("expected Shell, got {:?}", other),
        }
    }
}
