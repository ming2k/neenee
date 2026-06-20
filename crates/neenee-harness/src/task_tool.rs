//! `TaskTool` — spawns a read-only exploration sub-agent for research subtasks.
//!
//! Lives in `neenee-harness` (not `neenee-tools`) because it constructs an
//! [`crate::Agent`] internally: spawning a sub-agent is an orchestration
//! concern, not a domain-tool concern. The other tools (Bash/Read/Web/…)
//! stay in `neenee-tools` and remain pure trait implementations.

use std::sync::Arc;

use async_trait::async_trait;

use neenee_core::{AgentMode, Tool, ToolAccess};
use serde_json::json;

use crate::{agent::Agent, skills::SkillRegistry};

/// Spawn a read-only exploration sub-agent to handle a research sub-task.
///
/// The sub-agent runs the same provider with the read-only subset of tools,
/// so it never prompts for permission and cannot mutate the workspace. Its
/// final answer is returned to the calling agent, which stays in control of
/// any write operations. Recursion is prevented by excluding `task` (and
/// other dispatch tools) from the sub-agent's toolset.
pub struct TaskTool {
    provider: Arc<dyn neenee_core::Provider>,
    tools: Vec<Arc<dyn neenee_core::Tool>>,
}

impl TaskTool {
    /// `tools` should be the parent agent's full toolset; the task tool filters
    /// it down to read-only tools for the spawned sub-agent.
    pub fn new(provider: Arc<dyn neenee_core::Provider>, tools: Vec<Arc<dyn neenee_core::Tool>>) -> Self {
        Self { provider, tools }
    }
}

const TASK_MAX_ROUNDS_HINT: &str = "Run at most a handful of tool rounds, then answer.";

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Launch a focused, read-only sub-agent to research or explore part of the codebase (or the \
         web) and return a concise written answer. Use it to parallelize investigation: finding \
         where code lives, summarizing files, gathering context. The sub-agent cannot modify \
         files — you perform any edits after reviewing its findings."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "Short label for the sub-task (<=60 chars)" },
                "prompt": { "type": "string", "description": "The full, self-contained instructions for the sub-agent" }
            },
            "required": ["description", "prompt"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.run_sub_agent(arguments, Box::new(|_| {})).await
    }

    async fn call_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::SubTaskEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.run_sub_agent(arguments, on_event).await
    }

    async fn call_structured_with_events<'a>(
        &self,
        call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::SubTaskEvent) + Send + 'a>,
        _on_stream: &mut (dyn FnMut(neenee_core::ToolStream) + Send + 'a),
    ) -> Result<neenee_core::ToolOutput, String> {
        let _ = call_id;
        // Run the sub-agent, streaming its lifecycle as SubTaskEvents to the
        // parent harness (so the live TUI builds the nested view in real
        // time), then return a structured payload carrying the full transcript
        // + real token usage so the parent can persist children and account
        // cost truthfully.
        //
        // Failure path: a sub-agent that hit the 32-round limit, repeated-call
        // guard, or a provider error returns a Subagent payload too — its
        // `summary` is prefixed with `Error:` (so the existing failure
        // classification catches it) and the partial transcript is preserved
        // so the user can resume into the half-finished work and so the actual
        // token cost is accounted. Only input-validation errors (bad JSON,
        // missing fields) propagate as `Err`, because they have no partial
        // transcript worth keeping.
        let outcome = self.run_sub_agent_outcome(arguments, on_event).await?;
        let summary = if outcome.final_content.trim().is_empty() {
            "(sub-agent returned no answer)".to_string()
        } else {
            outcome.final_content.trim().to_string()
        };
        Ok(neenee_core::ToolOutput::Subagent {
            summary,
            messages: outcome.messages,
            usage: outcome.token_usage,
        })
    }
}

/// Internal result of running a sub-agent. Bundles everything the parent
/// harness needs to persist the nested transcript and account for real cost.
struct SubAgentOutcome {
    messages: Vec<neenee_core::Message>,
    token_usage: neenee_core::TokenUsage,
    /// Final assistant content, mirrored for convenience so the parent doesn't
    /// have to scan `messages` for the last Assistant turn.
    final_content: String,
}

impl TaskTool {
    async fn run_sub_agent_outcome<'a>(
        &self,
        arguments: &str,
        mut on_event: Box<dyn FnMut(neenee_core::SubTaskEvent) + Send + 'a>,
    ) -> Result<SubAgentOutcome, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let description = args["description"]
            .as_str()
            .ok_or("Missing 'description'")?
            .trim();
        let prompt = args["prompt"].as_str().ok_or("Missing 'prompt'")?;
        if description.is_empty() {
            return Err("'description' must not be empty.".to_string());
        }
        if prompt.trim().is_empty() {
            return Err("'prompt' must not be empty.".to_string());
        }

        // Sub-agent gets read-only tools only; never itself (no recursion).
        let sub_tools: Vec<Arc<dyn neenee_core::Tool>> = self
            .tools
            .iter()
            .filter(|tool| tool.access() == neenee_core::ToolAccess::Read && tool.name() != "task")
            .cloned()
            .collect();

        let goal_service = neenee_core::GoalService::new(
            neenee_core::GoalStore::open_in_memory()
                .await
                .map_err(|err| format!("failed to create sub-agent goal store: {err}"))?,
        );
        let sub_agent = Agent::new(
            self.provider.clone(),
            sub_tools,
            AgentMode::Build,
            goal_service,
            SkillRegistry::empty(),
        );

        let system = format!(
            "You are a focused research sub-agent. Your single job is to answer the assigned task \
             accurately and concisely using read-only tools. Explore the workspace or the web as \
             needed, then write a clear, complete final answer with the key findings (file paths, \
             signatures, relevant snippets, conclusions). Do not modify any files. {}\n\nTask: {}",
            TASK_MAX_ROUNDS_HINT, description,
        );
        let mut messages = vec![
            neenee_core::Message::new(neenee_core::Role::System, system),
            neenee_core::Message::new(neenee_core::Role::User, prompt.to_string()),
        ];
        // The sub-agent runs with its own (never-cancelled) token. When the
        // parent turn is interrupted, the parent's dispatch drops this future
        // and emits a `ToolCancelled` for the `task` call id; the TUI then
        // recursively cancels the nested tool steps, so the sub-agent does not
        // need a token linked to the parent.
        //
        // On failure we surface the partial transcript anyway — both so the
        // parent's tool-result message carries the sub-agent's work-in-progress
        // `children` and so the real token cost (which can be substantial for a
        // 32-round burnout) reaches the parent goal accounting. The
        // `final_content` is prefixed `Error: …` so the existing failure
        // classifier (`starts_with("Error")`) and the TUI's red Failed badge
        // both trigger.
        match sub_agent
            .run_streaming_with_events(
                &mut messages,
                &tokio_util::sync::CancellationToken::new(),
                |event| Self::forward_event(event, &mut on_event),
            )
            .await
        {
            Ok(result) => {
                let final_content = result.message.content.clone();
                Ok(SubAgentOutcome {
                    messages,
                    token_usage: result.token_usage,
                    final_content,
                })
            }
            Err(error) => {
                let error_string = error.to_string();
                tracing::warn!(error = %error_string, "sub-agent failed; preserving partial transcript");
                Ok(SubAgentOutcome {
                    messages,
                    token_usage: neenee_core::TokenUsage::default(),
                    final_content: format!("Error: {error_string}"),
                })
            }
        }
    }

    async fn run_sub_agent<'a>(
        &self,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::SubTaskEvent) + Send + 'a>,
    ) -> Result<String, String> {
        let outcome = self.run_sub_agent_outcome(arguments, on_event).await?;
        let content = outcome.final_content.trim().to_string();
        if content.is_empty() {
            Ok("(sub-agent returned no answer)".to_string())
        } else {
            Ok(content)
        }
    }

    fn forward_event(event: neenee_core::AgentEvent, on_event: &mut dyn FnMut(neenee_core::SubTaskEvent)) {
        match event {
            neenee_core::AgentEvent::ModelRequestStarted { tool_round } => {
                let status = if tool_round == 0 {
                    "waiting for model".to_string()
                } else {
                    format!("waiting for model · round {}", tool_round + 1)
                };
                on_event(neenee_core::SubTaskEvent::Activity(status));
            }
            neenee_core::AgentEvent::AssistantDelta { delta, start } => {
                if start {
                    on_event(neenee_core::SubTaskEvent::StreamStart);
                }
                on_event(neenee_core::SubTaskEvent::StreamDelta(delta));
            }
            neenee_core::AgentEvent::AssistantEnd(content) => {
                on_event(neenee_core::SubTaskEvent::StreamEnd(content));
            }
            neenee_core::AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                on_event(neenee_core::SubTaskEvent::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            neenee_core::AgentEvent::ToolResult {
                id,
                name,
                output,
                duration_ms,
                ..
            } => {
                on_event(neenee_core::SubTaskEvent::ToolResult {
                    id,
                    name,
                    output,
                    duration_ms,
                });
            }
            _ => {}
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use neenee_core::{Message, Provider, Role};
    use futures::stream::{self, BoxStream};

    struct CannedProvider;

    #[async_trait::async_trait]
    impl Provider for CannedProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Ok(Message::new(Role::Assistant, "found 3 relevant files"))
        }
        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::once(async {
                Ok("found 3 relevant files".to_string())
            })))
        }
    }

    struct EchoReadTool;

    #[async_trait::async_trait]
    impl Tool for EchoReadTool {
        fn name(&self) -> &str {
            "echo_read"
        }
        fn description(&self) -> &str {
            "test read tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn access(&self) -> ToolAccess {
            ToolAccess::Read
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("echo".to_string())
        }
    }

    #[tokio::test]
    async fn task_tool_runs_read_only_subagent_and_returns_answer() {
        let tool = TaskTool::new(
            std::sync::Arc::new(CannedProvider),
            vec![std::sync::Arc::new(EchoReadTool)],
        );

        let output = tool
            .call(r#"{"description":"find files","prompt":"where are the handlers?"}"#)
            .await
            .unwrap();

        assert_eq!(output, "found 3 relevant files");
    }

    #[tokio::test]
    async fn task_tool_rejects_missing_fields() {
        let tool = TaskTool::new(std::sync::Arc::new(CannedProvider), Vec::new());
        assert!(tool.call(r#"{"description":"x"}"#).await.is_err());
        assert!(tool.call(r#"{"prompt":"x"}"#).await.is_err());
    }
}
