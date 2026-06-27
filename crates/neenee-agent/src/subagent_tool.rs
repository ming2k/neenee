//! `SubagentTool` — spawns a read-only exploration subagent for research subtasks.
//!
//! Lives in `neenee-agent` (not `neenee-tools`) because it constructs an
//! [`crate::Agent`] internally: spawning a subagent is an orchestration
//! concern, not a domain-tool concern. The other tools (Bash/Read/Web/…)
//! stay in `neenee-tools` and remain pure trait implementations.
//!
//! Admission of tools to the subagent is driven by [`neenee_core::EXPLORE`]
//! — the single source of truth for the read-only / non-interactive /
//! non-recursive policy. See ADR-0011.

use std::sync::Arc;

use async_trait::async_trait;

use neenee_core::{SubagentProfile, Tool};
use serde_json::json;

use crate::agent::{Agent, SubagentHandle};
use crate::skills::SkillRegistry;

/// Live subagent handles keyed by the parent tool-call id — the lookup table
/// that lets the harness route a down-direction reply (a permission decision
/// or `ask_user` answer the user gave in the TUI) back into the specific
/// running subagent that surfaced the request. Full-duplex (ADR-0029).
///
/// The `task` tool populates this when it spawns a child (and clears the entry
/// when the child finishes); the harness reads it when it needs to reply to a
/// `SubagentEvent::PermissionRequest` / `UserQuestionRequest` that arrived
/// nested under a given `parent_call_id`. Entries are best-effort: a late reply
/// after the child already finished finds no entry (or a dead handle) and
/// degrades to a no-op rather than erroring.
#[derive(Default)]
pub struct SubagentRegistry {
    map: std::sync::Mutex<std::collections::HashMap<String, SubagentHandle>>,
}

impl SubagentRegistry {
    /// Register a steering handle for the subagent spawned by the
    /// `parent_call_id` tool call. Replaces any prior entry for that id.
    pub fn register(&self, parent_call_id: &str, handle: SubagentHandle) {
        #[allow(clippy::expect_used)]
        // lock poisoning means a panic already occurred in another holder
        self.map
            .lock()
            .expect("SubagentRegistry poisoned")
            .insert(parent_call_id.to_string(), handle);
    }

    /// Look up the handle for a live subagent by its parent tool-call id.
    /// Returns a cloned handle (cheap) so the caller can reply without holding
    /// the lock.
    pub fn get(&self, parent_call_id: &str) -> Option<SubagentHandle> {
        #[allow(clippy::expect_used)]
        // lock poisoning means a panic already occurred in another holder
        self.map
            .lock()
            .expect("SubagentRegistry poisoned")
            .get(parent_call_id)
            .cloned()
    }

    /// Remove the entry for a finished subagent. Called when the `task` tool
    /// returns, so the registry never accumulates dead handles for completed
    /// calls (a handle whose `Weak` already expired is harmless but useless).
    pub fn remove(&self, parent_call_id: &str) {
        #[allow(clippy::expect_used)]
        // lock poisoning means a panic already occurred in another holder
        self.map
            .lock()
            .expect("SubagentRegistry poisoned")
            .remove(parent_call_id);
    }
}

/// Spawn a read-only exploration subagent to handle a research sub-task.
///
/// The subagent runs the same provider with the tools admitted by the bound
/// [`SubagentProfile`] (today always [`neenee_core::EXPLORE`]): read-only, non-interactive,
/// non-recursive. Its final answer is returned to the calling agent, which
/// stays in control of any write operations and any questions for the user.
pub struct SubagentTool {
    provider: Arc<dyn neenee_core::Provider>,
    tools: Vec<Arc<dyn neenee_core::Tool>>,
    profile: &'static SubagentProfile,
    /// Full-duplex handle registry (ADR-0029): each spawned subagent's
    /// [`SubagentHandle`] is lodged here keyed by the parent tool-call id, so
    /// the harness can route a user's permission / `ask_user` reply back down
    /// into the exact child that surfaced the request. Owned by the tool and
    /// exposed via [`SubagentTool::registry`] so the binary that constructs the
    /// tool (and drives the harness) can hand the same `Arc` to the harness.
    registry: Arc<SubagentRegistry>,
}

impl SubagentTool {
    /// `tools` should be the parent agent's full toolset; `profile` declares
    /// what the spawned subagent may actually use (admission + framing). The
    /// caller binds the role explicitly — `&EXPLORE` for the `subagent` tool.
    pub fn new(
        provider: Arc<dyn neenee_core::Provider>,
        tools: Vec<Arc<dyn neenee_core::Tool>>,
        profile: &'static SubagentProfile,
    ) -> Self {
        Self {
            provider,
            tools,
            profile,
            registry: Arc::new(SubagentRegistry::default()),
        }
    }

    /// The shared handle registry for sub-agents spawned by this tool. The
    /// binary passes this `Arc` to the harness so a user reply in the TUI can
    /// be routed back into the live child (ADR-0029). Each `SubagentTool` instance
    /// owns its own registry (children of different dispatch tools are
    /// disjoint), which is fine because the harness that needs to reply is the
    /// same one that constructed the tool.
    pub fn registry(&self) -> Arc<SubagentRegistry> {
        self.registry.clone()
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }
    fn description(&self) -> &str {
        "Launch a focused, read-only subagent to research or explore part of the codebase (or the \
         web) and return a concise written answer. Use it to parallelize investigation: finding \
         where code lives, summarizing files, gathering context. The subagent cannot modify \
         files — you perform any edits after reviewing its findings."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "Short label for the sub-task (<=60 chars)" },
                "prompt": { "type": "string", "description": "The full, self-contained instructions for the subagent" }
            },
            "required": ["description", "prompt"]
        })
    }

    /// `task` spawns a subagent; subagent profiles exclude it to prevent
    /// unbounded recursion.
    fn spawns_subagent(&self) -> bool {
        true
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.run_sub_agent(None, arguments, Box::new(|_| {})).await
    }

    async fn call_with_events<'a>(
        &self,
        call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::SubagentEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.run_sub_agent(Some(call_id), arguments, on_event).await
    }

    async fn call_structured_with_events<'a>(
        &self,
        call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::SubagentEvent) + Send + 'a>,
        _on_stream: &mut (dyn FnMut(neenee_core::ToolStream) + Send + 'a),
    ) -> Result<neenee_core::ToolOutput, String> {
        // Run the subagent, streaming its lifecycle as SubAgentEvents to the
        // parent harness (so the live TUI builds the nested view in real
        // time), then return a structured payload carrying the full transcript
        // + real token usage so the parent can persist children and account
        // cost truthfully.
        //
        // `call_id` is now used (not discarded): it keys the child's duplex
        // handle in the registry (ADR-0029) so a user reply can flow back down
        // into this exact child while it runs.
        //
        // Failure path: a subagent that hit the 32-round limit, repeated-call
        // guard, or a provider error returns a Subagent payload too — the
        // structured `failed` flag is set so the UI classifies it as Failed
        // without text-sniffing, and the partial transcript is preserved so
        // the user can resume into the half-finished work and the real token
        // cost is accounted. The summary still carries an `Error:` prefix so
        // the parent *model* understands the sub-task did not succeed. Only
        // input-validation errors (bad JSON, missing fields) propagate as
        // `Err`, because they have no partial transcript worth keeping.
        let outcome = self
            .run_sub_agent_outcome(Some(call_id), arguments, on_event)
            .await?;
        let summary = if outcome.final_content.trim().is_empty() {
            if outcome.failed {
                "(subagent failed before producing an answer)".to_string()
            } else {
                "(subagent returned no answer)".to_string()
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

/// Internal result of running a subagent. Bundles everything the parent
/// harness needs to persist the nested transcript and account for real cost.
struct SubagentOutcome {
    messages: Vec<neenee_core::Message>,
    token_usage: neenee_core::TokenUsage,
    /// Final assistant content, mirrored for convenience so the parent doesn't
    /// have to scan `messages` for the last Assistant turn.
    final_content: String,
    /// Whether the subagent terminated abnormally (hit the tool-round cap,
    /// repeated-call guard, or a provider error). Drives the structured
    /// `failed` flag on the returned [`neenee_core::ToolOutput::Subagent`]
    /// instead of the old `summary.starts_with("Error")` text sniff.
    failed: bool,
}

impl SubagentTool {
    async fn run_sub_agent_outcome<'a>(
        &self,
        call_id: Option<&str>,
        arguments: &str,
        mut on_event: Box<dyn FnMut(neenee_core::SubagentEvent) + Send + 'a>,
    ) -> Result<SubagentOutcome, String> {
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

        // Announce the bound profile name first so the parent harness / TUI
        // can label this subagent by its role (explore / plan / verify / …)
        // rather than a generic "Subagent". Emitted before the child runs.
        on_event(neenee_core::SubagentEvent::Started {
            profile: self.profile.name.to_string(),
        });

        // Subagent tools come from the bound profile's policy — the single
        // source of truth for read-only / non-interactive / non-recursive
        // admission. See ADR-0011.
        let sub_tools: Vec<Arc<dyn neenee_core::Tool>> = self.profile.select_tools(&self.tools);

        // The subagent's identity *is* its profile's system prompt — that is the
        // persona/mission framing for this role (e.g. EXPLORE's research
        // framing). `from_persona` injects it verbatim as the preamble.
        let identity = crate::AgentIdentity::from_persona(self.profile.system_prompt);
        let sub_agent = Arc::new(Agent::new(
            self.provider.clone(),
            sub_tools,
            SkillRegistry::empty(),
            identity,
        ));
        // A `task` sub-agent runs unobstructed: disable the deterministic
        // read-loop guard's nudge (ADR-0034) so a short-lived, parent-supervised
        // sub-agent is never steered by it. The parent and `abort` remain its
        // backstops.
        sub_agent.set_loop_review_enabled(false);
        // Full-duplex (ADR-0029): install the child's steering inbox and lodge
        // its handle in the registry keyed by the parent tool-call id. Now any
        // permission / `ask_user` request the child surfaces travels *up* via
        // `forward_event`, and the user's reply can travel *down* via the
        // registry → handle → `reply_permission` / `reply_user_question`,
        // resolving the child's parked oneshot. A `None` call_id (the bare
        // `call` path, no harness involvement) skips registration — there is no
        // one to reply, so the child must stay self-contained.
        let _handle = sub_agent.install_inbox();
        if let Some(id) = call_id {
            self.registry.register(id, _handle.clone());
        }
        // Full-duplex (ADR-0029): the broker gate is now profile-driven. The
        // built-in profiles keep `auto_approve: true` to preserve the legacy
        // autonomous contract, but a profile with `auto_approve: false` lets a
        // subagent's write/execute tool calls surface as
        // `SubagentEvent::PermissionRequest` up to the parent, with the user's
        // reply routed back down via the registry → handle →
        // `reply_permission` (the parked oneshot resolves directly, no inbox
        // drain needed).
        sub_agent.set_auto_approve(self.profile.auto_approve);
        // Resolve the bound profile's write grant (ADR-0028) against the
        // process cwd and set it on the child. All built-in profiles
        // (EXPLORE/REVIEW/TITLE: empty `write_paths`) resolve to
        // `WriteScope::None`, consistent with their admission (no write tools
        // admitted anyway). The `INTERACTIVE` role carries an unrestricted
        // scope via its `Write` ceiling.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        sub_agent.set_operation_scope(self.profile.resolve_operation_scope(&cwd));
        // Sub-agents are short-lived and read-only by profile, and session
        // review is on-demand (`/review`) with no automatic firing — so a
        // research subagent never pays for a diagnostic and review can never
        // recurse. No setup needed here. ADR-0018.

        // The subagent's transcript opens with just the task as the user
        // message. The head system message is rebuilt every round by
        // `prepare_turn_messages` from the profile persona (carried via
        // `AgentIdentity`, set above) composed with the mission-neutral
        // sections (tone, todo) through the prompt registry — see ADR-0039.
        //
        // An earlier `Task: {description}` system message here was dead code:
        // `ensure_system_prompt` replaces any leading system message on round
        // 1, so it was clobbered before the first model request and the
        // persona (also vying for index 0) was what actually reached the
        // model. Dropping it makes the single-message path honest. The task
        // itself is the user message; `description` remains a required label
        // arg (validated above) for the parent / TUI.
        let mut messages = vec![neenee_core::Message::new(
            neenee_core::Role::User,
            prompt.to_string(),
        )];
        // The subagent runs with its own (never-cancelled) token. When the
        // parent turn is interrupted, the parent's dispatch drops this future
        // and emits a `ToolCancelled` for the `task` call id; the TUI then
        // recursively cancels the nested tool steps, so the subagent does not
        // need a token linked to the parent.
        //
        // On failure we surface the partial transcript anyway — both so the
        // parent's tool-result message carries the subagent's work-in-progress
        // `children` and so the real token cost (which can be substantial for a
        // 32-round burnout) reaches the parent pursuit accounting. The
        // `final_content` is prefixed `Error: …` so the existing failure
        // classifier (`starts_with("Error")`) and the TUI's red Failed badge
        // both trigger.
        let result = sub_agent
            .run_streaming_with_events(
                &mut messages,
                &tokio_util::sync::CancellationToken::new(),
                |event| Self::forward_event(event, &mut on_event),
            )
            .await;
        // Drop the registry entry for this call regardless of outcome so it
        // never holds a dead handle. The child `Arc` is also dropped here
        // (the last strong ref besides the registry's `Weak`), so any late
        // reply via the handle degrades to a no-op.
        if let Some(id) = call_id {
            self.registry.remove(id);
        }
        match result {
            Ok(result) => {
                let final_content = result.message.content.clone();
                Ok(SubagentOutcome {
                    messages,
                    token_usage: result.token_usage,
                    final_content,
                    failed: false,
                })
            }
            Err(error) => {
                let error_string = error.to_string();
                tracing::warn!(error = %error_string, "subagent failed; preserving partial transcript");
                Ok(SubagentOutcome {
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
        call_id: Option<&str>,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::SubagentEvent) + Send + 'a>,
    ) -> Result<String, String> {
        let outcome = self
            .run_sub_agent_outcome(call_id, arguments, on_event)
            .await?;
        let content = outcome.final_content.trim().to_string();
        if content.is_empty() {
            Ok("(subagent returned no answer)".to_string())
        } else {
            Ok(content)
        }
    }

    fn forward_event(
        event: neenee_core::AgentEvent,
        on_event: &mut dyn FnMut(neenee_core::SubagentEvent),
    ) {
        match event {
            neenee_core::AgentEvent::ModelRequestStarted { tool_round } => {
                let status = if tool_round == 0 {
                    "waiting for model".to_string()
                } else {
                    format!("waiting for model · round {}", tool_round + 1)
                };
                on_event(neenee_core::SubagentEvent::Activity(status));
            }
            neenee_core::AgentEvent::AssistantDelta { delta, start } => {
                if start {
                    on_event(neenee_core::SubagentEvent::StreamStart);
                }
                on_event(neenee_core::SubagentEvent::StreamDelta(delta));
            }
            neenee_core::AgentEvent::AssistantEnd(content) => {
                on_event(neenee_core::SubagentEvent::StreamEnd(content));
            }
            neenee_core::AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                on_event(neenee_core::SubagentEvent::ToolCall {
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
                on_event(neenee_core::SubagentEvent::ToolResult {
                    id,
                    name,
                    output,
                    duration_ms,
                });
            }
            // Full-duplex (ADR-0029): a permission broker request from the
            // child now travels *up* as a SubagentEvent so the parent harness
            // can surface it to the user. The reply travels back *down* via
            // the registry → handle → `reply_permission`, which resolves the
            // child's parked oneshot directly (no inbox drain needed). The
            // built-in profiles still suppress this in practice via
            // `auto_approve` + excluding `requires_user` tools, so reaching
            // here means either a future interactive profile is in use, or a
            // policy leak — forwarding (not dropping) is correct in both cases.
            neenee_core::AgentEvent::PermissionRequest(request) => {
                on_event(neenee_core::SubagentEvent::PermissionRequest(request));
            }
            // Same full-duplex contract as the permission arm above. Reaching
            // here means an `ask_user` tool was admitted (the profile allows
            // user interaction) and the child is parked awaiting answers.
            neenee_core::AgentEvent::UserQuestionRequest(request) => {
                on_event(neenee_core::SubagentEvent::UserQuestionRequest(request));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream};
    use neenee_core::{EXPLORE, Message, Provider, Role};

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
            "read_file"
        }
        fn description(&self) -> &str {
            "test read tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("echo".to_string())
        }
    }

    #[tokio::test]
    async fn task_tool_runs_read_only_subagent_and_returns_answer() {
        let tool = SubagentTool::new(
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

    /// Regression for ADR-0039 stage 3: the subagent's head system message is
    /// the registry-composed persona + mission-neutral sections (tone, todo).
    /// The legacy `Task: {description}` system message was dead code —
    /// `ensure_system_prompt` clobbered index 0 on round 1 — and has been
    /// removed; the task lives in the user message alone.
    #[tokio::test]
    async fn subagent_head_system_message_has_no_dead_task_line() {
        let tool = SubagentTool::new(
            std::sync::Arc::new(CannedProvider),
            vec![std::sync::Arc::new(EchoReadTool)],
            &EXPLORE,
        );
        let outcome = tool
            .run_sub_agent_outcome(
                None,
                r#"{"description":"find files","prompt":"where are the handlers?"}"#,
                Box::new(|_event: neenee_core::SubagentEvent| {}),
            )
            .await
            .unwrap();

        // messages[0] is the rebuilt system message: EXPLORE persona opens it,
        // then the todo section composes in.
        let system = &outcome.messages[0];
        assert_eq!(system.role, neenee_core::Role::System);
        assert!(
            system
                .content
                .starts_with("You are a focused research subagent"),
            "system message should open with the EXPLORE persona"
        );
        assert!(
            system.content.contains("Task tracking:"),
            "todo guidance section composes in"
        );
        assert!(
            !system.content.contains("Task: find files"),
            "the dead `Task: {{description}}` line must not appear (ADR-0039)"
        );

        // The task is the user message, untouched by the system assembly.
        assert_eq!(outcome.messages[1].role, neenee_core::Role::User);
        assert_eq!(outcome.messages[1].content, "where are the handlers?");
    }

    #[tokio::test]
    async fn task_tool_rejects_missing_fields() {
        let tool = SubagentTool::new(std::sync::Arc::new(CannedProvider), Vec::new(), &EXPLORE);
        assert!(tool.call(r#"{"description":"x"}"#).await.is_err());
        assert!(tool.call(r#"{"prompt":"x"}"#).await.is_err());
    }

    /// A non-whitelisted stub, used to prove the explore profile rejects tools
    /// by name (it is not in READ_ONLY_TOOLS).
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
        let subagent_tool = SubagentTool::new(provider.clone(), Vec::new(), &EXPLORE);

        let tools: Vec<std::sync::Arc<dyn Tool>> = vec![
            std::sync::Arc::new(EchoReadTool),
            std::sync::Arc::new(neenee_tools::AskUserTool),
            std::sync::Arc::new(StubWriteTool),
            std::sync::Arc::new(subagent_tool),
        ];

        let admitted = EXPLORE.select_tools(&tools);
        let admitted_names: Vec<&str> = admitted.iter().map(|t| t.name()).collect();

        assert_eq!(admitted_names, vec!["read_file"]);
    }

    /// Cross-cut regression: `EXPLORE` admits only its whitelisted read tools —
    /// `ask_user`, the non-whitelisted write stub, and recursion are all
    /// excluded. The read stub is admitted because it is named `read_file`,
    /// which is in [`READ_ONLY_TOOLS`].
    #[test]
    fn explore_profile_excludes_bash_writes_user_and_recursion() {
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(CannedProvider);
        let subagent_tool = SubagentTool::new(provider.clone(), Vec::new(), &EXPLORE);

        let tools: Vec<std::sync::Arc<dyn Tool>> = vec![
            std::sync::Arc::new(EchoReadTool),
            std::sync::Arc::new(neenee_tools::BashTool),
            std::sync::Arc::new(neenee_tools::AskUserTool),
            std::sync::Arc::new(StubWriteTool),
            std::sync::Arc::new(subagent_tool),
        ];

        // EXPLORE: only the whitelisted read tool survives (bash, ask_user,
        // the write stub, and recursion are all excluded).
        let explore_selected = EXPLORE.select_tools(&tools);
        let explore_names: Vec<&str> = explore_selected.iter().map(|t| t.name()).collect();
        assert_eq!(explore_names, vec!["read_file"]);
    }
}
