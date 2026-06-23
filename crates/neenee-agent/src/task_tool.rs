//! `TaskTool` — spawns a read-only exploration sub-agent for research subtasks.
//!
//! Lives in `neenee-agent` (not `neenee-tools`) because it constructs an
//! [`crate::Agent`] internally: spawning a sub-agent is an orchestration
//! concern, not a domain-tool concern. The other tools (Bash/Read/Web/…)
//! stay in `neenee-tools` and remain pure trait implementations.
//!
//! Admission of tools to the sub-agent is driven by [`neenee_core::EXPLORE`]
//! — the single source of truth for the read-only / non-interactive /
//! non-recursive policy. See ADR-0011.

use std::sync::Arc;

use async_trait::async_trait;

use neenee_core::{AgentMode, SubagentProfile, Tool, ToolAccess};
use serde_json::json;

use crate::{agent::Agent, skills::SkillRegistry};

/// Spawn a read-only exploration sub-agent to handle a research sub-task.
///
/// The sub-agent runs the same provider with the tools admitted by the bound
/// [`SubagentProfile`] (today always [`neenee_core::EXPLORE`]): read-only, non-interactive,
/// non-recursive. Its final answer is returned to the calling agent, which
/// stays in control of any write operations and any questions for the user.
pub struct TaskTool {
    provider: Arc<dyn neenee_core::Provider>,
    tools: Vec<Arc<dyn neenee_core::Tool>>,
    profile: &'static SubagentProfile,
}

impl TaskTool {
    /// `tools` should be the parent agent's full toolset; `profile` declares
    /// what the spawned sub-agent may actually use (admission + framing). The
    /// caller binds the role explicitly — `&EXPLORE` for the `task` tool,
    /// `&VERIFY` for the plan verifier — so the dispatch surface shows the
    /// intended sub-agent shape rather than hiding a default.
    pub fn new(
        provider: Arc<dyn neenee_core::Provider>,
        tools: Vec<Arc<dyn neenee_core::Tool>>,
        profile: &'static SubagentProfile,
    ) -> Self {
        Self {
            provider,
            tools,
            profile,
        }
    }
}

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

    /// `task` spawns a sub-agent; sub-agent profiles exclude it to prevent
    /// unbounded recursion.
    fn spawns_subagent(&self) -> bool {
        true
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
        // guard, or a provider error returns a Subagent payload too — the
        // structured `failed` flag is set so the UI classifies it as Failed
        // without text-sniffing, and the partial transcript is preserved so
        // the user can resume into the half-finished work and the real token
        // cost is accounted. The summary still carries an `Error:` prefix so
        // the parent *model* understands the sub-task did not succeed. Only
        // input-validation errors (bad JSON, missing fields) propagate as
        // `Err`, because they have no partial transcript worth keeping.
        let outcome = self.run_sub_agent_outcome(arguments, on_event).await?;
        let summary = if outcome.final_content.trim().is_empty() {
            if outcome.failed {
                "(sub-agent failed before producing an answer)".to_string()
            } else {
                "(sub-agent returned no answer)".to_string()
            }
        } else {
            outcome.final_content.trim().to_string()
        };
        Ok(neenee_core::ToolOutput::Subagent {
            summary,
            messages: outcome.messages,
            usage: outcome.token_usage,
            failed: outcome.failed,
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
    /// Whether the sub-agent terminated abnormally (hit the tool-round cap,
    /// repeated-call guard, or a provider error). Drives the structured
    /// `failed` flag on the returned [`neenee_core::ToolOutput::Subagent`]
    /// instead of the old `summary.starts_with("Error")` text sniff.
    failed: bool,
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

        // Sub-agent tools come from the bound profile's policy — the single
        // source of truth for read-only / non-interactive / non-recursive
        // admission. See ADR-0011.
        let sub_tools: Vec<Arc<dyn neenee_core::Tool>> = self.profile.select_tools(&self.tools);

        let pursuit_service = neenee_core::PursuitService::new(
            neenee_core::PursuitStore::open_in_memory()
                .await
                .map_err(|err| format!("failed to create sub-agent pursuit store: {err}"))?,
        );
        let sub_agent = Agent::new(
            self.provider.clone(),
            sub_tools,
            AgentMode::Build,
            pursuit_service,
            SkillRegistry::empty(),
        );

        let system = format!("{}\n\nTask: {}", self.profile.system_prompt, description);
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
        // 32-round burnout) reaches the parent pursuit accounting. The
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
                    failed: false,
                })
            }
            Err(error) => {
                let error_string = error.to_string();
                tracing::warn!(error = %error_string, "sub-agent failed; preserving partial transcript");
                Ok(SubAgentOutcome {
                    messages,
                    token_usage: neenee_core::TokenUsage::default(),
                    final_content: format!("Error: {error_string}"),
                    failed: true,
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

    fn forward_event(
        event: neenee_core::AgentEvent,
        on_event: &mut dyn FnMut(neenee_core::SubTaskEvent),
    ) {
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
            // The sub-agent has no user reachable to answer. The bound
            // profile excludes `requires_user` tools, so this branch should
            // be unreachable; if it fires, some interactive tool leaked past
            // the policy and the request would otherwise deadlock silently.
            // Log loudly so the invariant break is observable rather than
            // turning into a hang. See ADR-0011.
            neenee_core::AgentEvent::UserQuestionRequest(request) => {
                tracing::error!(
                    request_id = %request.id,
                    questions = request.questions.len(),
                    "sub-agent emitted a user-question request, which the task tool cannot \
                     forward to any user; dropping. The bound profile is meant to exclude \
                     interactive tools — this indicates a policy leak",
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream};
    use neenee_core::{Message, Provider, Role, EXPLORE};

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
            &EXPLORE,
        );

        let output = tool
            .call(r#"{"description":"find files","prompt":"where are the handlers?"}"#)
            .await
            .unwrap();

        assert_eq!(output, "found 3 relevant files");
    }

    #[tokio::test]
    async fn task_tool_rejects_missing_fields() {
        let tool = TaskTool::new(std::sync::Arc::new(CannedProvider), Vec::new(), &EXPLORE);
        assert!(tool.call(r#"{"description":"x"}"#).await.is_err());
        assert!(tool.call(r#"{"prompt":"x"}"#).await.is_err());
    }

    /// A Write-capable stub, used to prove the explore profile rejects write
    /// tools by capability rather than by name.
    struct StubWriteTool;

    #[async_trait::async_trait]
    impl Tool for StubWriteTool {
        fn name(&self) -> &str {
            "stub_write"
        }
        fn description(&self) -> &str {
            "test write tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn access(&self) -> ToolAccess {
            ToolAccess::Write
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("write".to_string())
        }
    }

    /// Regression for the deadlock fixed in ADR-0011: the explore profile
    /// must exclude (a) the real `ask_user` tool — Read but interactive,
    /// (b) any write tool, and (c) `task` itself — Read but a dispatch tool
    /// that would recurse. Built with the real tool instances the harness
    /// registers, not stubs, so a future capability-bit regression on either
    /// side is caught here.
    #[test]
    fn explore_profile_excludes_user_write_and_recursion_using_real_tools() {
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(CannedProvider);
        let task_tool = TaskTool::new(provider.clone(), Vec::new(), &EXPLORE);

        let tools: Vec<std::sync::Arc<dyn Tool>> = vec![
            std::sync::Arc::new(EchoReadTool),
            std::sync::Arc::new(neenee_tools::AskUserTool),
            std::sync::Arc::new(StubWriteTool),
            std::sync::Arc::new(task_tool),
        ];

        let admitted = EXPLORE.select_tools(&tools);
        let admitted_names: Vec<&str> = admitted.iter().map(|t| t.name()).collect();

        assert_eq!(admitted_names, vec!["echo_read"]);
    }

    /// Cross-cut regression for ADR-0012: the real `bash` tool is now
    /// `Execute`, so `EXPLORE` (Read ceiling) excludes it — an explorer must
    /// not run commands — while `VERIFY` (Execute ceiling) admits it so an
    /// independent plan verifier can run tests/builds/type-checks. `VERIFY`
    /// still drops the same dangerous trio (`ask_user`, write, recursion).
    #[test]
    fn verify_profile_admits_real_bash_but_not_writes_user_or_recursion() {
        use neenee_core::VERIFY;
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(CannedProvider);
        let task_tool = TaskTool::new(provider.clone(), Vec::new(), &VERIFY);

        let tools: Vec<std::sync::Arc<dyn Tool>> = vec![
            std::sync::Arc::new(EchoReadTool),
            std::sync::Arc::new(neenee_tools::BashTool),
            std::sync::Arc::new(neenee_tools::AskUserTool),
            std::sync::Arc::new(StubWriteTool),
            std::sync::Arc::new(task_tool),
        ];

        // EXPLORE: only the read tool survives (bash's Execute tier is above
        // the Read ceiling).
        let explore_selected = EXPLORE.select_tools(&tools);
        let explore_names: Vec<&str> = explore_selected.iter().map(|t| t.name()).collect();
        assert_eq!(explore_names, vec!["echo_read"]);

        // VERIFY: read + bash admitted; write / ask_user / task excluded.
        let verify_selected = VERIFY.select_tools(&tools);
        let verify_names: Vec<&str> = verify_selected.iter().map(|t| t.name()).collect();
        assert_eq!(verify_names, vec!["echo_read", "bash"]);
    }
}
