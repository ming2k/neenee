//! The command-handler hook implementation and registry builder (ADR-0025).
//!
//! Each `[hooks]` entry becomes one [`CommandHook`] that spawns a shell
//! process: the [`HookContext`] is serialized to JSON on stdin, and the
//! process replies via exit code and stdout JSON. This is the only handler
//! type v1 ships; the [`Hook`] trait (in `neenee_core`) is shaped so `http`
//! and `mcp_tool` handlers can be added later without touching the loop.

use std::path::Path;
use std::time::Duration;

use neenee_agent::async_trait;
use neenee_agent::{Hook, HookContext, HookEvent, HookEventKind, HookOutcome};
use neenee_store::config::HookSpec;
use serde_json::json;

/// Default per-hook timeout. A hook that does not finish in this window is
/// killed and treated as `Pass` (a non-blocking error), so a hung script never
/// wedges the agent loop. Generous enough for a linter or CI shard.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// A lifecycle hook that runs a shell command (ADR-0025). Built from a
/// [`HookSpec`]; the command runs with the project root as cwd and receives
/// the hook context as JSON on stdin.
#[derive(Debug)]
pub struct CommandHook {
    kind: HookEventKind,
    matcher: Option<String>,
    command: String,
}

impl CommandHook {
    pub fn from_spec(spec: &HookSpec) -> Self {
        Self {
            kind: spec.event,
            matcher: spec.matcher.clone(),
            command: spec.command.clone(),
        }
    }
}

#[async_trait]
impl Hook for CommandHook {
    fn kind(&self) -> HookEventKind {
        self.kind
    }

    fn matcher(&self) -> Option<&str> {
        self.matcher.as_deref()
    }

    async fn fire(&self, ctx: &HookContext) -> HookOutcome {
        let stdin_json = context_to_json(ctx);
        let cwd = ctx.cwd.as_deref().unwrap_or_else(|| Path::new("."));

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Inherit NEENEE_* and the ambient environment so scripts can reach
        // configured tools; nothing secret is added.
        let spawn = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(command = %self.command, ?error, "hook spawn failed");
                return HookOutcome::Pass;
            }
        };

        let result = match write_stdin_and_collect(spawn, stdin_json.as_bytes()).await {
            Ok(r) => r,
            Err(error) => {
                tracing::warn!(command = %self.command, ?error, "hook io failed");
                return HookOutcome::Pass;
            }
        };

        interpret_output(result)
    }
}

/// The captured result of running a hook command.
struct CommandResult {
    stdout: String,
    stderr: String,
    exit: Option<i32>,
}

/// Write `stdin_bytes` to the child's stdin, then await exit, collecting
/// stdout/stderr. Bounded by [`HOOK_TIMEOUT`].
async fn write_stdin_and_collect(
    mut child: tokio::process::Child,
    stdin_bytes: &[u8],
) -> std::io::Result<CommandResult> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Detach the stdio handles up front so the wait below does not deadlock
    // waiting on a pipe the child holds open while it waits on our stdin.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_bytes).await;
        let _ = stdin.flush().await;
        // Drop to signal EOF.
        drop(stdin);
    }

    // Read both streams to EOF off the wait path, so a child that writes a
    // large result then exits does not block on a full pipe.
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut stream) = stdout {
            let _ = stream.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut stream) = stderr {
            let _ = stream.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    match tokio::time::timeout(HOOK_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            Ok(CommandResult {
                stdout,
                stderr,
                exit: status.code(),
            })
        }
        Ok(Err(error)) => Err(error),
        Err(_) => {
            // Timed out; best-effort kill. `child.wait()` borrows, so the child
            // is still ours to kill here.
            let _ = child.start_kill();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "hook timed out",
            ))
        }
    }
}

/// Translate a command's exit code + stdout into a [`HookOutcome`].
///
/// - exit `2`: blocking deny; the stderr (or a default) is the reason fed back
///   to the model. Honoured on `PreToolUse` / `Stop`.
/// - stdout parses as a JSON object: `{"decision":"deny","reason":"…""}`
///   denies; `{"context":"…"}` injects; `{"decision":"approve"}` passes.
/// - anything else (exit 0 with no JSON, non-2 exit, parse failure): `Pass`.
///   A non-blocking error never aborts the loop — enforce hard rules with the
///   permission system, not a flaky script.
fn interpret_output(result: CommandResult) -> HookOutcome {
    if result.exit == Some(2) {
        let reason = result.stderr.trim();
        let reason = if reason.is_empty() {
            "blocked by hook".to_string()
        } else {
            reason.to_string()
        };
        return HookOutcome::Deny { reason };
    }

    let trimmed = result.stdout.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        if !result.stderr.is_empty() && !matches!(result.exit, Some(0) | None) {
            tracing::info!(
                exit = ?result.exit,
                stderr = %result.stderr.trim(),
                "hook exited non-zero (non-blocking)"
            );
        }
        return HookOutcome::Pass;
    }

    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value) => {
            let decision = value.get("decision").and_then(|v| v.as_str());
            match decision {
                Some("deny") => {
                    let reason = value
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("blocked by hook")
                        .to_string();
                    HookOutcome::Deny { reason }
                }
                Some("approve") => HookOutcome::Pass,
                _ => {
                    if let Some(context) = value.get("context").and_then(|v| v.as_str()) {
                        HookOutcome::Inject {
                            context: context.to_string(),
                        }
                    } else {
                        HookOutcome::Pass
                    }
                }
            }
        }
        Err(_) => HookOutcome::Pass,
    }
}

/// Serialize a [`HookContext`] to a flat JSON object convenient for shell
/// scripts (one level, `jq`-friendly) rather than a nested enum.
fn context_to_json(ctx: &HookContext) -> String {
    let mut value = json!({
        "session_id": ctx.session_id,
        "event": event_name(&ctx.event),
    });
    if let Some(cwd) = &ctx.cwd {
        value["cwd"] = json!(cwd.display().to_string());
    }
    match &ctx.event {
        HookEvent::SessionStart { source } => {
            value["source"] = json!(match source {
                neenee_agent::SessionSource::Startup => "startup",
                neenee_agent::SessionSource::Resume => "resume",
            });
        }
        HookEvent::SessionEnd => {}
        HookEvent::UserPromptSubmit { prompt } => {
            value["prompt"] = json!(prompt);
        }
        HookEvent::PreToolUse {
            tool_name,
            tool_input,
        } => {
            value["tool_name"] = json!(tool_name);
            value["tool_input"] = tool_input.clone();
        }
        HookEvent::PostToolUse {
            tool_name,
            tool_output,
            duration_ms,
        } => {
            value["tool_name"] = json!(tool_name);
            value["tool_output"] = json!(tool_output);
            value["duration_ms"] = json!(duration_ms);
        }
        HookEvent::PostToolUseFailure { tool_name, error } => {
            value["tool_name"] = json!(tool_name);
            value["error"] = json!(error);
        }
        HookEvent::Stop { last_message } => {
            value["last_message"] = json!(last_message);
        }
        HookEvent::Turn {
            turn,
            consecutive_readonly,
        } => {
            value["turn"] = json!(turn);
            value["consecutive_readonly"] = json!(consecutive_readonly);
        }
        HookEvent::PreCompact | HookEvent::PostCompact => {}
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn event_name(event: &HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart { .. } => "SessionStart",
        HookEvent::SessionEnd => "SessionEnd",
        HookEvent::UserPromptSubmit { .. } => "UserPromptSubmit",
        HookEvent::PreToolUse { .. } => "PreToolUse",
        HookEvent::PostToolUse { .. } => "PostToolUse",
        HookEvent::PostToolUseFailure { .. } => "PostToolUseFailure",
        HookEvent::Stop { .. } => "Stop",
        HookEvent::PreCompact => "PreCompact",
        HookEvent::PostCompact => "PostCompact",
        HookEvent::Turn { .. } => "Turn",
    }
}

/// Build the hook registry from the `[hooks]` config. Unknown/invalid specs
/// are skipped with a warning rather than aborting startup.
pub fn build_hook_registry(specs: &[HookSpec]) -> neenee_agent::HookRegistry {
    let hooks: Vec<std::sync::Arc<dyn Hook>> = specs
        .iter()
        .map(|spec| {
            let hook: std::sync::Arc<dyn Hook> = std::sync::Arc::new(CommandHook::from_spec(spec));
            tracing::info!(
                event = ?spec.event,
                matcher = ?spec.matcher,
                command = %spec.command,
                "registered hook"
            );
            hook
        })
        .collect();
    neenee_agent::HookRegistry::new(hooks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(stdout: &str, stderr: &str, exit: Option<i32>) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit,
        }
    }

    #[test]
    fn exit_2_denies_with_stderr_reason() {
        assert_eq!(
            interpret_output(result("", "nope", Some(2))),
            HookOutcome::Deny {
                reason: "nope".into()
            }
        );
    }

    #[test]
    fn exit_0_no_json_passes() {
        assert_eq!(interpret_output(result("", "", Some(0))), HookOutcome::Pass);
    }

    #[test]
    fn json_deny_wins_over_exit_code() {
        assert_eq!(
            interpret_output(result(r#"{"decision":"deny","reason":"bad"}"#, "", Some(0))),
            HookOutcome::Deny {
                reason: "bad".into()
            }
        );
    }

    #[test]
    fn json_context_injects() {
        assert_eq!(
            interpret_output(result(r#"{"context":"remember X"}"#, "", Some(0))),
            HookOutcome::Inject {
                context: "remember X".into()
            }
        );
    }

    #[test]
    fn invalid_json_passes() {
        assert_eq!(
            interpret_output(result("{not json", "", Some(0))),
            HookOutcome::Pass
        );
    }
}
