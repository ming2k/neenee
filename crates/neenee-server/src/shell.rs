//! The `!`-prefix shell-command path, extracted from `main.rs`. Executes a
//! command directly through the `bash` tool, bypassing the LLM, and emits the
//! same lifecycle events as a normal tool step (`ToolCall` → live
//! `ToolStream` → `ToolResult` / `ToolCancelled`) so the existing render path
//! picks it up unchanged.

use neenee_agent::Agent;
use neenee_agent::orchestration::{send_harness_state, turn};
use neenee_core::{AgentResponse, Tool, ToolOutput, ToolStream, TurnEvent};
use neenee_tools::BashTool;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Execute a `!`-prefixed shell command directly through the `bash` tool,
/// bypassing the LLM. Emits the same lifecycle events as a normal tool step
/// (`ToolCall` → live `ToolStream` → `ToolResult` or `ToolCancelled`) so the
/// existing render path picks it up unchanged.
///
/// Cancellation mirrors `start_interactive_turn`: a fresh
/// [`CancellationToken`] is installed (any previous token is cancelled) and
/// the generation counter is bumped so a later turn supersedes a still-running
/// shell command and its tail-end events do not race with the new turn.
pub async fn run_shell_command(
    command: String,
    tx: mpsc::UnboundedSender<AgentResponse>,
    session_id: String,
    token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_counter: Arc<AtomicU64>,
    agent: Arc<Agent>,
) {
    let call_id = format!("shell_{}", uuid::Uuid::new_v4());
    let arguments = serde_json::json!({ "command": command }).to_string();

    // Mirror start_interactive_turn: install a fresh cancellation token,
    // cancelling any in-flight predecessor, and bump the generation so we
    // can tell on exit whether we are still the active turn.
    let token = CancellationToken::new();
    if let Some(previous) = token_slot.write().await.replace(token.clone()) {
        agent.reject_pending_permissions();
        let _ = tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }
    let generation = generation_counter.fetch_add(1, Ordering::SeqCst) + 1;
    let is_current = || generation_counter.load(Ordering::SeqCst) == generation;

    // Surface the synthetic tool step starting. The response listener maps
    // `name: "bash"` to the "running command" activity status.
    let _ = tx.send(turn(
        &session_id,
        TurnEvent::ToolCall {
            id: call_id.clone(),
            name: "bash".to_string(),
            arguments: arguments.clone(),
        },
    ));

    let bash = BashTool;
    let tx_for_stream = tx.clone();
    let session_id_for_stream = session_id.clone();
    let call_id_for_stream = call_id.clone();
    let mut on_stream = move |stream: ToolStream| {
        if !is_current() {
            return;
        }
        let _ = tx_for_stream.send(turn(
            &session_id_for_stream,
            TurnEvent::ToolStream {
                id: call_id_for_stream.clone(),
                stream,
            },
        ));
    };

    // The `!` passthrough is a user-direct shell invocation. We use the safe
    // Closed default here too (consistent with the model-driven path); a
    // future enhancement may let the `!` channel opt into a PTY or human
    // input injection for truly interactive commands, but that is a separate
    // UX decision from the autonomous-agent stdin contract.
    let run = bash.call_structured_with_events(
        "",
        &arguments,
        Box::new(|_| {}),
        &mut on_stream,
        neenee_core::StdinPolicy::default(),
    );

    tokio::select! {
        biased;
        _ = token.cancelled() => {
            // Ctrl+C (or a newer turn replacing us): dropping `run` kills
            // the child via `kill_on_drop`. Only emit the cancellation
            // event if we are still the active turn — a newer turn's
            // ToolCall events must not be flattened by our exit.
            if is_current() {
                let _ = tx.send(turn(
                    &session_id,
                    TurnEvent::ToolCancelled {
                        id: call_id,
                        name: "bash".to_string(),
                    },
                ));
            }
        }
        result = run => if is_current() {
            match result {
                Ok(structured) => {
                    let output = structured.to_text();
                    let _ = tx.send(turn(
                        &session_id,
                        TurnEvent::ToolResult {
                            id: call_id,
                            name: "bash".to_string(),
                            output,
                            structured,
                            duration_ms: 0,
                        },
                    ));
                }
                Err(error) => {
                    let structured = ToolOutput::Text(error.clone());
                    let _ = tx.send(turn(
                        &session_id,
                        TurnEvent::ToolResult {
                            id: call_id,
                            name: "bash".to_string(),
                            output: error,
                            structured,
                            duration_ms: 0,
                        },
                    ));
                }
            }
        },
    }

    // Release the slot and flip the harness to idle, matching
    // start_interactive_turn's cleanup. Guarded by the generation check so
    // a newer turn is not reset by our exit.
    let mut slot = token_slot.write().await;
    if is_current() {
        slot.take();
        drop(slot);
        send_harness_state(&tx, &session_id, &agent, "idle");
        let _ = tx.send(turn(&session_id, TurnEvent::Activity(String::new())));
    }
}
