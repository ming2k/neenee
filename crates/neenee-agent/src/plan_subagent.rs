//! `PlanTool` — the main agent's `plan` tool (ADR-0027).
//!
//! It delegates planning to a read-only `PLAN` subagent that returns the full
//! plan markdown in its reply. The tool itself only spawns the subagent; the
//! harness-side `Agent::execute_plan` wrapper receives the markdown, derives a
//! slug + path, writes the file, gates the result behind user approval, then
//! seeds the todo list. Splitting it this way mirrors the legacy `plan_exit`
//! path: interactive approval needs `self.ask_user` + the event channel,
//! which only the agent (not a tool) can reach.
//!
//! Under the hood it reuses [`SubagentTool`] bound to [`neenee_core::PLAN`], so the
//! spawn, event-forwarding, and structured `Subagent` result are identical to
//! the `task` tool — only the bound profile and the `{request}` parameter
//! shape differ.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::{ PLAN, Provider, SubagentEvent, Tool, ToolAccess, ToolOutput, ToolStream};
use serde_json::json;

use crate::subagent_tool::SubagentTool;

/// The main agent's planning tool. Spawns a `PLAN` subagent to research and
/// write a plan; approval + todo seeding happen in [`Agent::execute_plan`].
pub struct PlanTool {
    inner: SubagentTool,
}

impl PlanTool {
    /// `tools` should be the parent agent's toolset; the `PLAN` profile selects
    /// the read tools plus the (scoped) write tools from it.
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            inner: SubagentTool::new(provider, tools, &PLAN),
        }
    }
}

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &str {
        "plan"
    }
    fn description(&self) -> &str {
        "Delegate planning for a non-trivial change to a focused, read-only plan subagent. \
         The subagent researches the codebase, designs an implementation approach, and writes \
         the plan to .neenee/plans/<slug>.md; the user then approves it before you implement. \
         Use this when work spans multiple files, has several valid approaches, or needs design \
         before editing. For simple or well-specified edits, make them directly instead."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "request": {
                    "type": "string",
                    "description": "The change to plan, with enough surrounding context (files, goals, constraints) for the subagent to research and design it self-contained."
                }
            },
            "required": ["request"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    /// `plan` spawns a subagent; subagent profiles exclude it to prevent
    /// recursion, exactly like `task`.
    fn spawns_subagent(&self) -> bool {
        true
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let translated = translate_request(arguments)?;
        self.inner.call(&translated).await
    }

    async fn call_structured_with_events<'a>(
        &self,
        call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(SubagentEvent) + Send + 'a>,
        on_stream: &mut (dyn FnMut(ToolStream) + Send + 'a),
    ) -> Result<ToolOutput, String> {
        let translated = translate_request(arguments)?;
        self.inner
            .call_structured_with_events(call_id, &translated, on_event, on_stream)
            .await
    }
}

/// Translate the `plan` tool's `{request}` argument into the
/// `{description, prompt}` shape the inner [`SubagentTool`] expects. The `PLAN`
/// profile's system prompt already frames the role; the prompt just carries
/// the request verbatim, and the description is a short label for the sub-task
/// view.
fn translate_request(arguments: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
    let request = value
        .get("request")
        .and_then(|r| r.as_str())
        .ok_or("Missing 'request'")?;
    if request.trim().is_empty() {
        return Err("'request' must not be empty.".to_string());
    }
    let description: String = request.chars().take(60).collect();
    Ok(json!({ "description": description, "prompt": request }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream};
    use neenee_core::{Message, Role};

    struct CannedProvider;

    #[async_trait::async_trait]
    impl Provider for CannedProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Ok(Message::new(
                Role::Assistant,
                "# Plan\n\n## Summary\n- do the thing",
            ))
        }
        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::once(async {
                Ok("# Plan\n\n## Summary\n- do the thing".to_string())
            })))
        }
    }

    #[test]
    fn translate_request_round_trips_into_task_shape() {
        let out = translate_request(r#"{"request":"add auth"}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["prompt"], "add auth");
        assert_eq!(v["description"], "add auth");
    }

    #[test]
    fn translate_request_rejects_missing_and_empty() {
        assert!(translate_request(r#"{"description":"x"}"#).is_err());
        assert!(translate_request(r#"{"request":"  "}"#).is_err());
        assert!(translate_request("not json").is_err());
    }

    /// The `plan` tool spawns a `PLAN` subagent and surfaces its final reply.
    /// Under the new contract the reply *is* the plan markdown (no path
    /// signal); the approval + file-write + seeding happen harness-side and
    /// are not exercised here.
    #[tokio::test]
    async fn plan_tool_runs_subagent_and_returns_markdown() {
        let tool = PlanTool::new(std::sync::Arc::new(CannedProvider), Vec::new());
        let out = tool
            .call(r#"{"request":"plan the auth feature"}"#)
            .await
            .unwrap();
        assert!(out.contains("## Summary"));
    }
}
