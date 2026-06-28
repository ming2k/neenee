//! The model's self-initiated emergency escape hatch.
//!
//! `abort` lets the model stop the program when it detects a stuck state it
//! cannot recover from — a tool loop, a dangerous/irreversible operation it
//! has been pushed into, or a dead end. Calling it sends an
//! [`AgentRequest::Abort`], which the harness turns into a turn cancellation
//! (identical path to the user hitting `Esc`) followed by an
//! `AgentResponse::Exit`, so the normal graceful-exit sequence runs — the
//! session is saved and `SessionEnd` hooks fire — before the process and its
//! background tasks end.
//!
//! ## Why this exists
//!
//! The harness keeps no hard equality-guard *abort* (the old guard was removed).
//! The deterministic read-loop guard (ADR-0034) breaks repeated-*read* loops
//! automatically, but only by nudging — it never stops the program, and it does
//! not cover every stuck state (a dangerous irreversible operation, a non-read
//! dead end). For those a stuck model could otherwise spin until the user
//! notices and interrupts. This tool gives it an *active* way out: rather than
//! waiting to be rescued, it can bail on its own judgement.
//!
//! ## Capability axis
//!
//! This is a *control-flow* tool, not a filesystem tool. It declares
//! [`Tool::affects_control_flow`] = `true`, which is an orthogonal axis to
//! `Tool::access` (the filesystem-damage ladder). That flag, not `access()`,
//! is what gates it: the permission broker is bypassed (an escape hatch that
//! waits for approval is useless), and envoy profiles exclude it
//! unconditionally — a spawned agent must never be able to tear down the whole
//! program. `access()` is left at the `Write` default but is meaningless here.
//!
//! See `Tool::affects_control_flow` in `neenee-core::capability`.

use async_trait::async_trait;
use neenee_core::{AgentRequest, Tool};
use serde_json::json;
use tokio::sync::mpsc;

/// Emergency abort/exit tool. Holds a clone of the request channel so it can
/// signal the harness from within a tool call.
pub struct AbortTool {
    tx: mpsc::UnboundedSender<AgentRequest>,
}

impl AbortTool {
    pub fn new(tx: mpsc::UnboundedSender<AgentRequest>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl Tool for AbortTool {
    fn name(&self) -> &str {
        "abort"
    }
    fn description(&self) -> &str {
        "Emergency escape hatch: stop the operation and exit neenee. \
         Use ONLY when stuck in an unrecoverable loop or dead end. \
         Do not use for normal task completion."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Why you are aborting (e.g. 'stuck repeating webfetch', \
        'destructive command requested'). Recorded for the user."
                }
            },
            "required": ["reason"]
        })
    }
    // Meaningless for a control tool, but kept out of the Write-mutation
    // broker ceiling so an EXPLORE-profile agent could in principle reach it
    // (the envoy exclusion is enforced via affects_control_flow, below).
    // The orthogonal control-flow axis — this is the flag that actually gates
    // the tool (envoys are excluded by it; the broker is bypassed).
    fn affects_control_flow(&self) -> bool {
        true
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let reason = args["reason"].as_str().ok_or("Missing 'reason'")?.trim();
        let reason = if reason.is_empty() {
            "(no reason given)".to_string()
        } else {
            reason.to_string()
        };
        // Fire the abort. The harness cancels the in-flight turn (the very turn
        // executing this tool) and then sends AgentResponse::Exit so the TUI
        // shuts down gracefully. Because the turn is cancelled, this Ok value
        // may never become a tool_result message — which is the desired
        // "stop now" semantics.
        self.tx
            .send(AgentRequest::Abort)
            .map_err(|_| "abort channel closed".to_string())?;
        // Best-effort: if the turn is not cancelled before this returns (e.g.
        // the round is mid-batch), surface a final line for the transcript.
        Ok(format!("Aborting: {reason}. neenee will exit."))
    }
}

neenee_core::register_tool!(AbortFactory => |ctx| {
    // Pull the harness request channel out of the context by type. The CLI
    // provides it via `builder.provide(tx)` in main.rs. If absent (e.g. the
    // tool is constructed in a test context without a channel), fall back to a
    // disconnected sender so the tool still builds — a call on it will simply
    // report the channel closed.
    let tx = ctx
        .get::<mpsc::UnboundedSender<AgentRequest>>()
        .cloned()
        .unwrap_or_else(|| mpsc::unbounded_channel::<AgentRequest>().0);
    AbortTool::new(tx)
});

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abort_sends_request_and_surfaces_reason() {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentRequest>();
        let tool = AbortTool::new(tx);
        let out = tool
            .call(r#"{"reason":"stuck in a webfetch loop"}"#)
            .await
            .unwrap();
        // The request fired.
        assert!(matches!(rx.recv().await, Some(AgentRequest::Abort)));
        // The reason is echoed back for the transcript.
        assert!(out.contains("stuck in a webfetch loop"));
    }

    #[tokio::test]
    async fn abort_requires_reason_field() {
        let (tx, _rx) = mpsc::unbounded_channel::<AgentRequest>();
        let tool = AbortTool::new(tx);
        // Missing the field entirely is a usage error.
        assert!(tool.call(r#"{}"#).await.is_err());
    }

    #[test]
    fn abort_is_a_control_flow_tool() {
        let (tx, _rx) = mpsc::unbounded_channel::<AgentRequest>();
        let tool = AbortTool::new(tx);
        // The orthogonal control-flow axis — the flag that actually gates this
        // tool (envoy exclusion + broker bypass), not filesystem access.
        assert!(tool.affects_control_flow());
    }
}
