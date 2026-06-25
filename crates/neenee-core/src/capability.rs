//! Foundational capability traits: how the harness talks to a model
//! ([`Provider`]) and to tools ([`Tool`]), the stream events a provider emits
//! ([`ProviderStreamEvent`]), and the mid-turn context-relief hook
//! ([`ContextReliefGate`]).

use crate::{Message, SubagentEvent, ToolOutput, ToolStream};
use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String>;
    async fn stream_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String>;
    async fn stream_chat_events(
        &self,
        messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        Ok(self
            .stream_chat(messages)
            .await?
            .filter_map(|item| async move {
                match item {
                    Ok(delta) if delta.is_empty() => None,
                    Ok(delta) => Some(Ok(ProviderStreamEvent::TextDelta(delta))),
                    Err(error) => Some(Err(error)),
                }
            })
            .boxed())
    }

    /// Called by the agent before each turn so the provider can prepare tool schemas.
    /// Default is a no-op for providers that don't support native function calling.
    fn prepare_tools(&self, _tools: &[Arc<dyn Tool>]) {}

    /// Stable provider/solution identifier (e.g. `"kimi-code"`, `"gemini"`).
    /// The harness stamps it onto assistant messages so a session that mixes
    /// multiple models stays traceable. Defaults to an empty string for
    /// providers (mostly test doubles) that don't carry an identity.
    ///
    /// Returns an owned [`String`] because the active provider may live behind
    /// a runtime-swappable proxy that cannot lend out a borrow across its lock.
    fn provider_id(&self) -> String {
        String::new()
    }
    /// The model identifier this provider targets (e.g. `"kimi-k2.7-code"`).
    /// Companion to [`Provider::provider_id`]; defaults to an empty string.
    fn model(&self) -> String {
        String::new()
    }

    /// Toggle network capture for debugging. When `enabled` is true, every
    /// request flowing through this provider is serialized — request messages,
    /// the streamed/returned response, provider id, model, and a timestamp — to
    /// one JSON file under `dir` (one file per round-trip). When `enabled` is
    /// false, capture stops and `dir` is ignored. Default is a no-op; the
    /// runtime proxy ([`ProxyProvider`]) overrides it so capture survives
    /// mid-session `/provider` swaps. See the `/debug network` command.
    ///
    /// This lives at the semantic layer (`Vec<Message>` in / events out), not
    /// the HTTP byte layer: request URLs, headers, and transport bytes are not
    /// captured — by design, to avoid leaking API keys (e.g. providers that put
    /// the key in the query string) and to stay independent of each provider's
    /// HTTP client.
    fn set_debug_capture(&self, _enabled: bool, _dir: PathBuf) {}

    /// Whether network capture is currently armed on this provider. Defaults to
    /// `false`; the runtime proxy overrides it to report the live toggle state.
    fn debug_capture_enabled(&self) -> bool {
        false
    }
}

/// Mid-turn context-relief hook. After each tool round, when context pressure
/// crosses the agent's configured budget, the harness hands the live message
/// list to the gate and asks it to relieve pressure (e.g. by pruning old tool
/// results durably). Returning `Some(replacement)` swaps the live message list;
/// returning `None` leaves it untouched. The gate owns durability policy
/// (archiving originals before the replacement takes effect).
#[async_trait]
pub trait ContextReliefGate: Send + Sync {
    async fn relieve_pressure(&self, messages: Vec<Message>) -> Option<Vec<Message>>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }

    /// Whether executing this tool may block awaiting a live human decision
    /// (e.g. `ask_user`, an approval-gated mode switch). Non-interactive
    /// execution contexts — sub-agents spawned for autonomous research — have
    /// no user reachable to answer, so a [`crate::subagent::ToolPolicy`] with
    /// `allow_user_interaction: false` excludes these. See ADR-0011.
    fn requires_user(&self) -> bool {
        false
    }

    /// Whether invoking this tool spawns a nested agent. Subagent profiles
    /// exclude these unconditionally to prevent unbounded recursion — the
    /// outermost dispatch tool (`task`) and wrappers around it
    /// (`verify_plan_execution`) override to `true`. See ADR-0011.
    fn spawns_subagent(&self) -> bool {
        false
    }

    fn permission_scope(&self, _arguments: &str) -> String {
        "*".to_string()
    }

    /// Short, human-friendly label shown as the title of the permission
    /// prompt for `Write` tools. Defaults to the raw [`Tool::name`], which is
    /// fine when the name itself reads as a label (e.g. `bash`, `write_file`).
    /// Override when the name is a synthetic identifier whose meaning is not
    /// obvious to a user (e.g. `start_pursuit` -> `"Create pursuit"`). Only
    /// consulted for tools that actually trigger a permission prompt.
    ///
    /// This is purely a UI string; it never reaches the model and is not
    /// part of the function schema sent to providers.
    fn permission_label(&self) -> String {
        self.name().to_string()
    }

    /// User-facing description shown in the body of the permission prompt
    /// (the "Details" section). Defaults to [`Tool::description`], which is
    /// appropriate when that text is written for humans. Override when
    /// [`Tool::description`] is model-facing instruction prose (instructions
    /// like "do not infer pursuits from ordinary tasks") that would confuse a
    /// user reading the prompt. Keep overrides to one or two plain sentences
    /// describing *what the call does*, not *when the model should call it*.
    ///
    /// Like [`permission_label`](Self::permission_label), this never reaches
    /// the model.
    fn permission_description(&self) -> String {
        self.description().to_string()
    }

    async fn call(&self, arguments: &str) -> Result<String, String>;

    /// Structured result. Default delegates to [`call`](Self::call), wrapping
    /// the text as [`ToolOutput::Text`]. Tools override this to return richer
    /// variants (e.g. a shell exit code, a file patch) so callers render from
    /// data instead of string-sniffing. See ADR-0001. Migration is additive:
    /// unmigrated tools keep working through this default.
    async fn call_structured(&self, arguments: &str) -> Result<ToolOutput, String> {
        self.call(arguments).await.map(ToolOutput::text)
    }

    /// Structured, event-emitting execution — the method the harness actually
    /// invokes so typed output reaches the transcript. Default delegates to
    /// [`call_structured`](Self::call_structured) and emits no events. Tools
    /// that spawn sub-agents (e.g. `task`) override this to forward child
    /// events while still returning a [`ToolOutput`] (typically [`ToolOutput::Text`]).
    async fn call_structured_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(SubagentEvent) + Send + 'a>,
        _on_stream: &mut (dyn FnMut(ToolStream) + Send + 'a),
    ) -> Result<ToolOutput, String> {
        self.call_structured(arguments).await
    }

    /// Execute the tool while optionally emitting events (e.g. subagent steps).
    ///
    /// The default implementation simply calls `call()` and emits no events.
    /// Tools that spawn sub-agents can override this to stream child events back
    /// to the parent harness.
    async fn call_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(SubagentEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.call(arguments).await
    }

    /// Generate an OpenAI-compatible function schema for this tool.
    fn to_openai_function(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }
}

/// A tool's capability class, ordered `Read < Execute < Write`. Each consumer
/// of the axis expresses its rule as a threshold rather than a binary, so the
/// surfaces that consult it compose cleanly:
///
/// - **Permission broker** — prompts for any tool with `access() > Read`
///   (i.e. `Execute` or `Write`): both have side effects the user should
///   approve.
/// - **Write-scope gate** — a per-agent [`WriteScope`] boundary blocks write
///   tools whose target is outside the agent's granted paths (e.g. an
///   `INTERACTIVE` subagent scoped to the working tree). See ADR-0028.
/// - **Subagent profiles** — a [`crate::subagent::ToolPolicy`] sets an access
///   *ceiling*; a tool is admitted when `tool.access() <= policy.access`, or
///   when it is a write tool covered by a `write_paths` grant. `EXPLORE`
///   (ceiling `Read`) gets pure read tools; `INTERACTIVE` (ceiling `Write`)
///   admits the full ladder including scoped writes. See ADR-0012/0028.
///
/// Variant order is load-bearing: it defines the ordering used by the derived
/// `Ord`. Do not reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolAccess {
    /// Inspects state with no side effects (e.g. `read_file`, `grep`).
    Read,
    /// Runs commands and may have external side effects, but the tool itself
    /// is not a workspace-mutation primitive (e.g. `bash`). Broker-gated.
    Execute,
    /// The tool's purpose is to mutate the workspace (e.g. `write_file`,
    /// `edit_file`). Broker-gated on the main agent; scoped by [`WriteScope`]
    /// on sub-agents.
    Write,
}

/// Runtime filesystem-write boundary for an agent. A **hard capability limit,
/// not a prompt**: writes outside the scope are blocked outright. Orthogonal
/// to [`ToolAccess`], which admits *whether* a tool runs; `WriteScope` scopes
/// *where* an admitted write tool may land. See ADR-0028.
///
/// The main agent carries [`WriteScope::Unrestricted`] (the broker is still
/// the interactive layer inside it); a subagent carries the scope resolved
/// from its profile's `write_paths` grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteScope {
    /// No writes permitted (read-only / execute-only agents).
    None,
    /// Writes permitted only under these canonicalized directory prefixes.
    Scoped(Vec<std::path::PathBuf>),
    /// Writes permitted anywhere — the main agent.
    Unrestricted,
}

impl Default for WriteScope {
    /// The main agent is unrestricted by default; sub-agents override this at
    /// spawn via [`crate::subagent::SubagentProfile::resolve_write_scope`].
    fn default() -> Self {
        WriteScope::Unrestricted
    }
}

impl WriteScope {
    /// Whether a write to `path_str` (the value a write tool returns from
    /// [`Tool::permission_scope`]) is permitted under this scope.
    /// [`WriteScope::Unrestricted`] admits everything; [`WriteScope::None`]
    /// admits nothing; [`WriteScope::Scoped`] canonicalizes the target's
    /// parent and re-appends the file name (so a not-yet-existing file still
    /// resolves) and checks it starts with one of the granted directories.
    pub fn allows(&self, path_str: &str) -> bool {
        match self {
            WriteScope::Unrestricted => true,
            WriteScope::None => false,
            WriteScope::Scoped(dirs) => match resolve_for_check(path_str) {
                Some(target) => dirs.iter().any(|dir| target.starts_with(dir)),
                None => false,
            },
        }
    }
}

/// Resolve a (relative or absolute) path for a prefix-containment check: join
/// to the cwd, canonicalize the parent directory and re-append the file name
/// so a new file that does not exist yet still resolves. Mirrors the
/// plan-path resolver in `plan.rs`.
fn resolve_for_check(path: &str) -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    let p = Path::new(path);
    let cwd = std::env::current_dir().ok()?;
    // Path::join with an absolute path replaces the base, so absolute inputs
    // are handled correctly too.
    let parent = p.parent();
    let file_name = p.file_name();
    let resolved = match (parent, file_name) {
        (Some(parent), Some(file_name)) if !parent.as_os_str().is_empty() => {
            let abs_parent = cwd.join(parent);
            let canon_parent = abs_parent.canonicalize().unwrap_or(abs_parent);
            canon_parent.join(file_name)
        }
        _ => {
            let abs: PathBuf = cwd.join(p);
            abs.canonicalize().unwrap_or(abs)
        }
    };
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::WriteScope;
    use std::path::PathBuf;

    #[test]
    fn unrestricted_allows_everything_and_none_allows_nothing() {
        assert!(WriteScope::Unrestricted.allows("anywhere/x.rs"));
        assert!(WriteScope::Unrestricted.allows(""));
        assert!(!WriteScope::None.allows("anywhere/x.rs"));
    }

    #[test]
    fn scoped_allows_under_granted_dir_and_blocks_outside() {
        // Simulate resolve_write_scope's output: a canonical dir prefix.
        let cwd = std::env::current_dir().unwrap();
        let granted: PathBuf = cwd.join("output");
        let scope = WriteScope::Scoped(vec![granted.clone()]);

        // A new file under the granted dir resolves to granted/file and is allowed,
        // even though neither the dir nor the file exists yet.
        assert!(scope.allows(&granted.join("result.md").display().to_string()));
        // A path outside the granted dir is blocked.
        assert!(!scope.allows(&cwd.join("src/main.rs").display().to_string()));
    }
}
