//! `EnvoyTool` — spawns a read-only exploration envoy for research subtasks.
//!
//! Lives in `neenee-agent` (not `neenee-tools`) because it constructs an
//! [`crate::Agent`] internally: spawning an envoy is an orchestration
//! concern, not a domain-tool concern. The other tools (Bash/Read/Web/…)
//! stay in `neenee-tools` and remain pure trait implementations.
//!
//! Admission of tools to the envoy is driven by [`neenee_core::EXPLORE`]
//! — the single source of truth for the read-only / non-interactive /
//! non-recursive policy. See ADR-0011.

use std::sync::Arc;

use async_trait::async_trait;

use neenee_core::{EnvoyProfile, Tool};
use serde_json::json;

use crate::agent::{Agent, EnvoyHandle};
use crate::skills::SkillRegistry;

/// Live envoy handles keyed by the parent tool-call id — the lookup table
/// that lets the harness route a down-direction reply (a permission decision
/// or `ask_user` answer the user gave in the TUI) back into the specific
/// running envoy that surfaced the request. Full-duplex (ADR-0029).
///
/// The `task` tool populates this when it spawns a child (and clears the entry
/// when the child finishes); the harness reads it when it needs to reply to a
/// `EnvoyEvent::PermissionRequest` / `UserQuestionRequest` that arrived
/// nested under a given `parent_call_id`. Entries are best-effort: a late reply
/// after the child already finished finds no entry (or a dead handle) and
/// degrades to a no-op rather than erroring.
#[derive(Default)]
pub struct EnvoyRegistry {
    map: std::sync::Mutex<std::collections::HashMap<String, EnvoyHandle>>,
}

impl EnvoyRegistry {
    /// Register a steering handle for the envoy spawned by the
    /// `parent_call_id` tool call. Replaces any prior entry for that id.
    pub fn register(&self, parent_call_id: &str, handle: EnvoyHandle) {
        // Poison-recovery idiom (codebase convention): a panic in another
        // holder poisoned the lock; recover the inner data rather than
        // panicking on a second, downstream error.
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(parent_call_id.to_string(), handle);
    }

    /// Look up the handle for a live envoy by its parent tool-call id.
    /// Returns a cloned handle (cheap) so the caller can reply without holding
    /// the lock.
    pub fn get(&self, parent_call_id: &str) -> Option<EnvoyHandle> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(parent_call_id)
            .cloned()
    }

    /// Remove the entry for a finished envoy. Called when the `task` tool
    /// returns, so the registry never accumulates dead handles for completed
    /// calls (a handle whose `Weak` already expired is harmless but useless).
    pub fn remove(&self, parent_call_id: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(parent_call_id);
    }
}

/// Spawn a read-only exploration envoy to handle a research sub-task.
///
/// The envoy runs the same provider with the tools admitted by the bound
/// [`EnvoyProfile`] (today always [`neenee_core::EXPLORE`]): read-only, non-interactive,
/// non-recursive. Its final answer is returned to the calling agent, which
/// stays in control of any write operations and any questions for the user.
pub struct EnvoyTool {
    provider: Arc<dyn neenee_core::Provider>,
    toolset: neenee_core::ToolSet,
    profile: &'static EnvoyProfile,
    /// Shared handle to the parent agent's variant selection (the **override**
    /// axis). Bound after the parent agent is built (see
    /// [`EnvoyTool::bind_variant_selection`]). At spawn the child resolves
    /// its scoped capabilities to the model's chosen variants by snapshotting
    /// this, so an envoy — an agent on the same model — inherits the parent's
    /// overrides. `None` (the default, e.g. in tests) means default variants.
    parent_variants: std::sync::Mutex<Option<Arc<std::sync::Mutex<neenee_core::VariantSelection>>>>,
    /// Full-duplex handle registry (ADR-0029): each spawned envoy's
    /// [`EnvoyHandle`] is lodged here keyed by the parent tool-call id, so
    /// the harness can route a user's permission / `ask_user` reply back down
    /// into the exact child that surfaced the request. Owned by the tool and
    /// exposed via [`EnvoyTool::registry`] so the binary that constructs the
    /// tool (and drives the harness) can hand the same `Arc` to the harness.
    registry: Arc<EnvoyRegistry>,
}

impl EnvoyTool {
    /// `toolset` should be the parent agent's full capability set; `profile`
    /// declares what the spawned envoy may actually use (admission + variant
    /// pins + framing). The caller binds the role explicitly — `&EXPLORE` for
    /// the `envoy` tool.
    pub fn new(
        provider: Arc<dyn neenee_core::Provider>,
        toolset: neenee_core::ToolSet,
        profile: &'static EnvoyProfile,
    ) -> Self {
        Self {
            provider,
            toolset,
            profile,
            parent_variants: std::sync::Mutex::new(None),
            registry: Arc::new(EnvoyRegistry::default()),
        }
    }

    /// Bind the parent agent's variant-selection handle (the **override** axis)
    /// so spawned envoys inherit the model's tool overrides. Called once,
    /// after the parent agent is constructed (the agent owns the handle). When
    /// unbound, envoys use each capability's default variant.
    pub fn bind_variant_selection(
        &self,
        handle: Arc<std::sync::Mutex<neenee_core::VariantSelection>>,
    ) {
        *self
            .parent_variants
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Snapshot the parent's current variant selection (empty when unbound).
    fn variant_snapshot(&self) -> neenee_core::VariantSelection {
        self.parent_variants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|h| h.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }

    /// The shared handle registry for envoys spawned by this tool. The
    /// binary passes this `Arc` to the harness so a user reply in the TUI can
    /// be routed back into the live child (ADR-0029). Each `EnvoyTool` instance
    /// owns its own registry (children of different dispatch tools are
    /// disjoint), which is fine because the harness that needs to reply is the
    /// same one that constructed the tool.
    pub fn registry(&self) -> Arc<EnvoyRegistry> {
        self.registry.clone()
    }
}

#[async_trait]
impl Tool for EnvoyTool {
    fn name(&self) -> &str {
        "envoy"
    }
    fn description(&self) -> &str {
        "Launch a focused, read-only envoy to research or explore part of the codebase (or the \
         web) and return a concise written answer. Use it to parallelize investigation: finding \
         where code lives, summarizing files, gathering context. The envoy cannot modify \
         files — you perform any edits after reviewing its findings."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "Short label for the sub-task (<=60 chars)" },
                "prompt": { "type": "string", "description": "The full, self-contained instructions for the envoy" }
            },
            "required": ["description", "prompt"]
        })
    }

    /// `task` spawns an envoy; envoy profiles exclude it to prevent
    /// unbounded recursion.
    fn spawns_envoy(&self) -> bool {
        true
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.run_envoy(None, arguments, Box::new(|_| {})).await
    }

    async fn call_with_events<'a>(
        &self,
        call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::EnvoyEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.run_envoy(Some(call_id), arguments, on_event).await
    }

    async fn call_structured_with_events<'a>(
        &self,
        call_id: &str,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::EnvoyEvent) + Send + 'a>,
        _on_stream: &mut (dyn FnMut(neenee_core::ToolStream) + Send + 'a),
        _stdin: neenee_core::StdinPolicy,
    ) -> Result<neenee_core::ToolOutput, String> {
        // Run the envoy, streaming its lifecycle as EnvoyEvents to the
        // parent harness (so the live TUI builds the nested view in real
        // time), then return a structured payload carrying the full transcript
        // + real token usage so the parent can persist children and account
        // cost truthfully.
        //
        // `call_id` is now used (not discarded): it keys the child's duplex
        // handle in the registry (ADR-0029) so a user reply can flow back down
        // into this exact child while it runs.
        //
        // Failure path: an envoy that hit the 32-round limit, repeated-call
        // guard, or a provider error returns an Envoy payload too — the
        // structured `failed` flag is set so the UI classifies it as Failed
        // without text-sniffing, and the partial transcript is preserved so
        // the user can resume into the half-finished work and the real token
        // cost is accounted. The summary still carries an `Error:` prefix so
        // the parent *model* understands the sub-task did not succeed. Only
        // input-validation errors (bad JSON, missing fields) propagate as
        // `Err`, because they have no partial transcript worth keeping.
        let outcome = self
            .run_envoy_outcome(Some(call_id), arguments, on_event)
            .await?;
        let summary = if outcome.final_content.trim().is_empty() {
            if outcome.failed {
                "(envoy failed before producing an answer)".to_string()
            } else {
                "(envoy returned no answer)".to_string()
            }
        } else {
            outcome.final_content.trim().to_string()
        };
        Ok(neenee_core::ToolOutput::Envoy {
            summary,
            messages: outcome.messages,
            usage: outcome.token_usage,
            failed: outcome.failed,
        })
    }
}

/// Internal result of running an envoy. Bundles everything the parent
/// harness needs to persist the nested transcript and account for real cost.
struct EnvoyOutcome {
    messages: Vec<neenee_core::Message>,
    token_usage: neenee_core::TokenUsage,
    /// Final assistant content, mirrored for convenience so the parent doesn't
    /// have to scan `messages` for the last Assistant turn.
    final_content: String,
    /// Whether the envoy terminated abnormally (hit the tool-round cap,
    /// repeated-call guard, or a provider error). Drives the structured
    /// `failed` flag on the returned [`neenee_core::ToolOutput::Envoy`]
    /// instead of the old `summary.starts_with("Error")` text sniff.
    failed: bool,
}

impl EnvoyTool {
    async fn run_envoy_outcome<'a>(
        &self,
        call_id: Option<&str>,
        arguments: &str,
        mut on_event: Box<dyn FnMut(neenee_core::EnvoyEvent) + Send + 'a>,
    ) -> Result<EnvoyOutcome, String> {
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
        // can label this envoy by its role (explore / plan / verify / …)
        // rather than a generic "Envoy". Emitted before the child runs.
        on_event(neenee_core::EnvoyEvent::Started {
            profile: self.profile.name.to_string(),
        });

        // Resolve the pool for this envoy: profile selection ⊓ model selection.
        // The envoy is an agent on the *same* model as the parent, so it carries
        // the parent's model (capability limits + variant overrides). The profile
        // contributes the role scope and any variant pins; the model contributes
        // its variant overrides (snapshotted from the parent) and its hard
        // capability limits. `resolve_tools` composes both and applies the envoy
        // runtime hard rules (no recursion / control-flow / blocking-on-user).
        let model = neenee_core::resolve_model(&self.provider.model());
        let model_sel =
            neenee_core::ToolSelection::unrestricted().with_variants(self.variant_snapshot());
        let sub_tools = self
            .profile
            .resolve_tools(&self.toolset, &model, &model_sel);

        // The envoy's identity *is* its profile's system prompt — that is the
        // persona/mission framing for this role (e.g. EXPLORE's research
        // framing). `from_persona` injects it verbatim as the preamble.
        let identity = crate::AgentIdentity::from_persona(self.profile.system_prompt);
        let envoy = Arc::new(Agent::new(
            self.provider.clone(),
            sub_tools,
            SkillRegistry::empty(),
            identity,
        ));
        // A `task` envoy runs unobstructed: disable the deterministic
        // read-loop guard's nudge (ADR-0034) so a short-lived, parent-supervised
        // envoy is never steered by it. The parent and `abort` remain its
        // backstops.
        envoy.set_nudge_config(neenee_core::NudgeConfig::disabled());
        // Full-duplex (ADR-0029): install the child's steering inbox and lodge
        // its handle in the registry keyed by the parent tool-call id. Now any
        // permission / `ask_user` request the child surfaces travels *up* via
        // `forward_event`, and the user's reply can travel *down* via the
        // registry → handle → `reply_permission` / `reply_user_question`,
        // resolving the child's parked oneshot. A `None` call_id (the bare
        // `call` path, no harness involvement) skips registration — there is no
        // one to reply, so the child must stay self-contained.
        let _handle = envoy.install_inbox();
        if let Some(id) = call_id {
            self.registry.register(id, _handle.clone());
        }
        // Full-duplex (ADR-0029): the broker gate is now profile-driven. The
        // built-in profiles keep `unattended: true` to preserve the legacy
        // autonomous contract, but a profile with `unattended: false` lets a
        // envoy's write/execute tool calls surface as
        // `EnvoyEvent::PermissionRequest` up to the parent, with the user's
        // reply routed back down via the registry → handle →
        // `reply_permission` (the parked oneshot resolves directly, no inbox
        // drain needed).
        envoy.set_unattended(self.profile.unattended);
        // Resolve the bound profile's write grant (ADR-0028) against the
        // process cwd and set it on the child. All built-in profiles
        // (EXPLORE/REVIEW/TITLE: empty `write_paths`) resolve to
        // `WriteScope::None`, consistent with their admission (no write tools
        // admitted anyway). The `INTERACTIVE` role carries an unrestricted
        // scope via its `Write` ceiling.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        envoy.set_operation_scope(self.profile.resolve_operation_scope(&cwd));
        // Envoys are short-lived and read-only by profile, and session
        // review is on-demand (`/review`) with no automatic firing — so a
        // research envoy never pays for a diagnostic and review can never
        // recurse. No setup needed here. ADR-0018.

        // The envoy's transcript opens with just the task as the user
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
        // The envoy runs with its own (never-cancelled) token. When the
        // parent turn is interrupted, the parent's dispatch drops this future
        // and emits a `ToolCancelled` for the `task` call id; the TUI then
        // recursively cancels the nested tool steps, so the envoy does not
        // need a token linked to the parent.
        //
        // On failure we surface the partial transcript anyway — both so the
        // parent's tool-result message carries the envoy's work-in-progress
        // `children` and so the real token cost (which can be substantial for a
        // 32-round burnout) reaches the parent pursuit accounting. The
        // `final_content` is prefixed `Error: …` so the existing failure
        // classifier (`starts_with("Error")`) and the TUI's red Failed badge
        // both trigger.
        let result = envoy
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
                Ok(EnvoyOutcome {
                    messages,
                    token_usage: result.token_usage,
                    final_content,
                    failed: false,
                })
            }
            Err(error) => {
                let error_string = error.to_string();
                tracing::warn!(error = %error_string, "envoy failed; preserving partial transcript");
                Ok(EnvoyOutcome {
                    messages,
                    token_usage: neenee_core::TokenUsage::default(),
                    final_content: format!("Error: {error_string}"),
                    failed: true,
                })
            }
        }
    }

    async fn run_envoy<'a>(
        &self,
        call_id: Option<&str>,
        arguments: &str,
        on_event: Box<dyn FnMut(neenee_core::EnvoyEvent) + Send + 'a>,
    ) -> Result<String, String> {
        let outcome = self.run_envoy_outcome(call_id, arguments, on_event).await?;
        let content = outcome.final_content.trim().to_string();
        if content.is_empty() {
            Ok("(envoy returned no answer)".to_string())
        } else {
            Ok(content)
        }
    }

    fn forward_event(
        event: neenee_core::AgentEvent,
        on_event: &mut dyn FnMut(neenee_core::EnvoyEvent),
    ) {
        match event {
            neenee_core::AgentEvent::Notice(notice) => {
                on_event(neenee_core::EnvoyEvent::Notice(notice));
            }
            neenee_core::AgentEvent::ModelRequestStarted { tool_round } => {
                let status = if tool_round == 0 {
                    "waiting for model".to_string()
                } else {
                    format!("waiting for model · round {}", tool_round + 1)
                };
                on_event(neenee_core::EnvoyEvent::Activity(status));
            }
            neenee_core::AgentEvent::AssistantDelta { delta, start } => {
                if start {
                    on_event(neenee_core::EnvoyEvent::StreamStart);
                }
                on_event(neenee_core::EnvoyEvent::StreamDelta(delta));
            }
            neenee_core::AgentEvent::AssistantEnd(content) => {
                on_event(neenee_core::EnvoyEvent::StreamEnd(content));
            }
            neenee_core::AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                on_event(neenee_core::EnvoyEvent::ToolCall {
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
                on_event(neenee_core::EnvoyEvent::ToolResult {
                    id,
                    name,
                    output,
                    duration_ms,
                });
            }
            // Full-duplex (ADR-0029): a permission broker request from the
            // child now travels *up* as a EnvoyEvent so the parent harness
            // can surface it to the user. The reply travels back *down* via
            // the registry → handle → `reply_permission`, which resolves the
            // child's parked oneshot directly (no inbox drain needed). The
            // built-in profiles still suppress this in practice via
            // `unattended` + excluding `requires_user` tools, so reaching
            // here means either a future interactive profile is in use, or a
            // policy leak — forwarding (not dropping) is correct in both cases.
            neenee_core::AgentEvent::PermissionRequest(request) => {
                on_event(neenee_core::EnvoyEvent::PermissionRequest(request));
            }
            // Same full-duplex contract as the permission arm above. Reaching
            // here means an `ask_user` tool was admitted (the profile allows
            // user interaction) and the child is parked awaiting answers.
            neenee_core::AgentEvent::UserQuestionRequest(request) => {
                on_event(neenee_core::EnvoyEvent::UserQuestionRequest(request));
            }
            // L3.5 β: an interactive `bash` inside the envoy needs operator
            // input; forward the request up so the parent harness can surface
            // it, with the reply routed back down via `reply_input`.
            neenee_core::AgentEvent::InputRequest(request) => {
                on_event(neenee_core::EnvoyEvent::InputRequest(request));
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
            "read_text"
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

    /// A terse `read_text` variant and a write tool, to prove an envoy
    /// resolves the *model's* variant (override axis) and then narrows to the
    /// *profile's* scope (scope axis) — the two are orthogonal.
    struct TerseReadTool;
    #[async_trait::async_trait]
    impl Tool for TerseReadTool {
        fn name(&self) -> &str {
            "read_text"
        }
        fn variant(&self) -> &str {
            "terse"
        }
        fn description(&self) -> &str {
            "terse read tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("terse".to_string())
        }
    }
    #[test]
    fn envoy_inherits_model_variant_then_applies_profile_scope() {
        // `StubWriteTool` (name "stub_write") is not in EXPLORE's read-only
        // scope, so it is always excluded; `read_text` has two variants.
        let toolset = neenee_core::ToolSet::from_tools([
            std::sync::Arc::new(EchoReadTool) as std::sync::Arc<dyn Tool>,
            std::sync::Arc::new(TerseReadTool) as std::sync::Arc<dyn Tool>,
            std::sync::Arc::new(StubWriteTool) as std::sync::Arc<dyn Tool>,
        ]);
        let tool = EnvoyTool::new(std::sync::Arc::new(CannedProvider), toolset, &EXPLORE);

        let resolve = |tool: &EnvoyTool| {
            let model = neenee_core::resolve_model(&CannedProvider.model());
            let model_sel =
                neenee_core::ToolSelection::unrestricted().with_variants(tool.variant_snapshot());
            tool.profile
                .resolve_tools(&tool.toolset, &model, &model_sel)
        };

        // Unbound (no model override) → read_text resolves to its default
        // variant; the out-of-scope write tool is excluded regardless.
        let scoped = resolve(&tool);
        let read = scoped.iter().find(|t| t.name() == "read_text");
        assert_eq!(read.map(|t| t.variant()), Some("default"));
        assert!(scoped.iter().all(|t| t.name() != "stub_write"));

        // Bind a model selection pinning read_text=terse: the envoy inherits
        // the override (terse), while scope is still profile-driven.
        let mut sel = neenee_core::VariantSelection::new();
        sel.insert("read_text".to_string(), "terse".to_string());
        tool.bind_variant_selection(std::sync::Arc::new(std::sync::Mutex::new(sel)));
        let scoped = resolve(&tool);
        let read = scoped.iter().find(|t| t.name() == "read_text");
        assert_eq!(read.map(|t| t.variant()), Some("terse"));
        assert!(scoped.iter().all(|t| t.name() != "stub_write"));
    }

    #[tokio::test]
    async fn task_tool_runs_read_only_envoy_and_returns_answer() {
        let tool = EnvoyTool::new(
            std::sync::Arc::new(CannedProvider),
            neenee_core::ToolSet::from_tools([
                std::sync::Arc::new(EchoReadTool) as std::sync::Arc<dyn Tool>
            ]),
            &EXPLORE,
        );

        let output = tool
            .call(r#"{"description":"find files","prompt":"where are the handlers?"}"#)
            .await
            .unwrap();

        assert_eq!(output, "found 3 relevant files");
    }

    /// Regression for ADR-0039 stage 3: the envoy's head system message is
    /// the registry-composed persona + mission-neutral sections (tone, todo).
    /// The legacy `Task: {description}` system message was dead code —
    /// `ensure_system_prompt` clobbered index 0 on round 1 — and has been
    /// removed; the task lives in the user message alone.
    #[tokio::test]
    async fn envoy_head_system_message_has_no_dead_task_line() {
        let tool = EnvoyTool::new(
            std::sync::Arc::new(CannedProvider),
            neenee_core::ToolSet::from_tools([
                std::sync::Arc::new(EchoReadTool) as std::sync::Arc<dyn Tool>
            ]),
            &EXPLORE,
        );
        let outcome = tool
            .run_envoy_outcome(
                None,
                r#"{"description":"find files","prompt":"where are the handlers?"}"#,
                Box::new(|_event: neenee_core::EnvoyEvent| {}),
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
                .starts_with("You are a focused research envoy"),
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
        let tool = EnvoyTool::new(
            std::sync::Arc::new(CannedProvider),
            neenee_core::ToolSet::default(),
            &EXPLORE,
        );
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
        let envoy_tool =
            EnvoyTool::new(provider.clone(), neenee_core::ToolSet::default(), &EXPLORE);

        let toolset = neenee_core::ToolSet::from_tools(vec![
            std::sync::Arc::new(EchoReadTool) as std::sync::Arc<dyn Tool>,
            std::sync::Arc::new(neenee_tools::AskUserTool),
            std::sync::Arc::new(StubWriteTool),
            std::sync::Arc::new(envoy_tool),
        ]);

        let model = neenee_core::resolve_model(&CannedProvider.model());
        let model_sel = neenee_core::ToolSelection::unrestricted();
        let admitted = EXPLORE.resolve_tools(&toolset, &model, &model_sel);
        let admitted_names: Vec<&str> = admitted.iter().map(|t| t.name()).collect();

        assert_eq!(admitted_names, vec!["read_text"]);
    }

    /// Cross-cut regression: `EXPLORE` admits only its whitelisted read tools —
    /// `ask_user`, the non-whitelisted write stub, and recursion are all
    /// excluded. The read stub is admitted because it is named `read_text`,
    /// which is in [`READ_ONLY_TOOLS`].
    #[test]
    fn explore_profile_excludes_bash_writes_user_and_recursion() {
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(CannedProvider);
        let envoy_tool =
            EnvoyTool::new(provider.clone(), neenee_core::ToolSet::default(), &EXPLORE);

        let toolset = neenee_core::ToolSet::from_tools(vec![
            std::sync::Arc::new(EchoReadTool) as std::sync::Arc<dyn Tool>,
            std::sync::Arc::new(neenee_tools::BashTool),
            std::sync::Arc::new(neenee_tools::AskUserTool),
            std::sync::Arc::new(StubWriteTool),
            std::sync::Arc::new(envoy_tool),
        ]);

        // EXPLORE: only the whitelisted read tool survives (bash, ask_user,
        // the write stub, and recursion are all excluded).
        let model = neenee_core::resolve_model(&CannedProvider.model());
        let model_sel = neenee_core::ToolSelection::unrestricted();
        let explore_selected = EXPLORE.resolve_tools(&toolset, &model, &model_sel);
        let explore_names: Vec<&str> = explore_selected.iter().map(|t| t.name()).collect();
        assert_eq!(explore_names, vec!["read_text"]);
    }
}
