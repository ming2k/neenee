//! Foundational capability traits: how the harness talks to a model
//! ([`Provider`]) and to tools ([`Tool`]), the stream events a provider emits
//! ([`ProviderStreamEvent`]), and the mid-turn model-context projection hook
//! ([`ContextProjectionGate`]).

use crate::{Message, SubagentEvent, ToolOutput, ToolStream};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Per-model overrides for one tool: an optional replacement `description` and
/// an optional set of per-parameter overrides. When the agent builds a
/// provider's tool schemas for the active model, the `description` replaces the
/// tool's built-in [`Tool::description`], and each parameter override is
/// *deep-merged* into the tool's `parameters` JSON Schema — only the named
/// fields on the named parameters change; everything else is preserved. This is
/// how a single toolset can be re-worded and re-constrained to play to a
/// particular model's strengths (or quirks) without forking the tool
/// implementations.
///
/// Configured per model id under `[tool_overrides."<model-id>"]` in
/// `config.toml`; the agent selects the entry matching `Provider::model()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOverride {
    /// Replacement for the tool's built-in `description`. Omit to keep the
    /// built-in text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-parameter overrides, keyed by parameter name. Each value is
    /// deep-merged into the corresponding entry under
    /// `parameters.properties.<name>`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, serde_json::Map<String, serde_json::Value>>,
}

/// Per-model tool overrides: maps a tool's name to its [`ToolOverride`]. When
/// the agent builds a provider's tool schemas for the active model, an entry
/// here is applied to that tool's function schema (description replacement +
/// parameter deep-merge); the tool name and un-overridden fields are untouched.
///
/// Configured per model id under `[tool_overrides."<model-id>"]` in
/// `config.toml`; the agent selects the map matching `Provider::model()`.
pub type ToolOverrides = HashMap<String, ToolOverride>;

/// A shared empty [`ToolOverrides`] map, handy as a default borrow target so
/// callers can always hand out `&ToolOverrides` without an `Option`.
pub fn empty_tool_overrides() -> &'static ToolOverrides {
    static EMPTY: std::sync::LazyLock<ToolOverrides> = std::sync::LazyLock::new(ToolOverrides::new);
    &EMPTY
}

/// Deep-merge `patch` into `target` in place. For each key in `patch`: if both
/// `target[key]` and `patch[key]` are objects, recurse; otherwise `patch`
/// overwrites `target[key]`. Used to fold per-parameter overrides into a tool's
/// `parameters` JSON Schema so only the named fields change.
pub fn deep_merge_json(target: &mut serde_json::Value, patch: &serde_json::Value) {
    use serde_json::Value;
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (key, patch_val) in p {
                match t.get_mut(key) {
                    Some(Value::Object(_)) if patch_val.is_object() => {
                        if let Some(child) = t.get_mut(key) {
                            deep_merge_json(child, patch_val);
                        }
                    }
                    _ => {
                        t.insert(key.clone(), patch_val.clone());
                    }
                }
            }
        }
        // Non-object target: patch wins outright (replaces).
        (target, patch) => *target = patch.clone(),
    }
}

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

    /// Called by the agent before each turn so the provider can prepare tool
    /// schemas, with per-model description overrides. When a tool's name is
    /// present in `overrides`, its built-in description is replaced in the
    /// generated function schema. The default delegates to
    /// [`prepare_tools`](Self::prepare_tools), so providers that haven't opted
    /// in simply ignore overrides. Providers that build function schemas
    /// (OpenAI-/Anthropic-compatible) override this to thread overrides into
    /// [`Tool::to_openai_function_with`].
    fn prepare_tools_with(&self, tools: &[Arc<dyn Tool>], _overrides: &ToolOverrides) {
        self.prepare_tools(tools);
    }

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
    /// runtime proxy (`ProxyProvider`) overrides it so capture survives
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

/// Mid-turn model-context projection hook. After each tool round, when context
/// pressure crosses the configured budget, the harness hands the live message
/// list to the gate and asks it to produce the next model-visible window. A
/// `Some(replacement)` swaps the live message list; `None` leaves it untouched.
/// The gate owns durability policy: original content is archived before the
/// replacement takes effect.
#[async_trait]
pub trait ContextProjectionGate: Send + Sync {
    async fn project_context(&self, messages: Vec<Message>) -> Option<Vec<Message>>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

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

    /// Whether this tool exercises control over the harness itself (e.g. the
    /// abort/exit escape hatch), as opposed to the workspace/filesystem. This
    /// is orthogonal to [`Tool::scope_target`]: `scope_target` classifies *what
    /// the call touches*, while this classifies *process control*. Subagent
    /// profiles exclude control tools unconditionally — a spawned agent must
    /// never be able to tear down the whole program. A control tool bypasses
    /// the permission broker and scope gate entirely: it declares no
    /// [`ScopeTarget`] (the default [`ScopeTarget::Unspecified`]), so neither
    /// the scope gate nor the broker fires for it — it is gated solely by this
    /// flag.
    fn affects_control_flow(&self) -> bool {
        false
    }

    /// The operation target this call acts on, so the operation-scope gate can
    /// decide whether the call falls inside the agent's granted scope.
    ///
    /// Tools return a typed [`ScopeTarget`]: a file path for `write_file`/
    /// `edit_file`, the command string for `bash`, etc. The scope gate
    /// dispatches on the variant — `Path` targets are checked against the
    /// granted directory prefixes, `Command` targets against a command
    /// allowlist. [`ScopeTarget::Unspecified`] (the default) is admitted
    /// without a scope check, since the tool declares no locatable target.
    ///
    /// Like [`permission_label`](Self::permission_label), this never reaches
    /// the model.
    fn scope_target(&self, _arguments: &str) -> ScopeTarget {
        ScopeTarget::Unspecified
    }

    /// Short, human-friendly label shown as the title of the permission
    /// prompt for `Write` tools. Defaults to the raw [`Tool::name`], which is
    /// fine when the name itself reads as a label (e.g. `bash`, `write_file`).
    /// Override when the name is a synthetic identifier whose meaning is not
    /// obvious to a user. Only consulted for tools that actually trigger a
    /// permission prompt.
    ///
    /// This is purely a UI string; it never reaches the model and is not
    /// part of the function schema sent to providers.
    fn permission_label(&self) -> String {
        self.name().to_string()
    }

    /// User-facing description shown in the body of the permission prompt
    /// (the "Details" section). Defaults to [`Tool::description`], which is
    /// appropriate when that text is written for humans. Override when
    /// [`Tool::description`] is model-facing instruction prose (constraints
    /// aimed at the model rather than a description of the call's effect)
    /// that would confuse a user reading the prompt. Keep overrides to one
    /// or two plain sentences describing *what the call does*, not *when
    /// the model should call it*.
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

    /// Like [`to_openai_function`](Self::to_openai_function), but applies
    /// per-model overrides: if `overrides` contains an entry for this tool's
    /// name, its `description` (if any) replaces the built-in one, and each
    /// parameter override is deep-merged into the `parameters` JSON Schema. An
    /// empty or miss-having `overrides` map yields the same schema as the plain
    /// method.
    fn to_openai_function_with(&self, overrides: &ToolOverrides) -> serde_json::Value {
        let mut parameters = self.parameters();
        let description = match overrides.get(self.name()) {
            Some(ToolOverride {
                description: Some(desc),
                params,
            }) => {
                if !params.is_empty() {
                    apply_param_overrides(&mut parameters, params);
                }
                desc.as_str()
            }
            Some(ToolOverride {
                description: None,
                params,
            }) => {
                if !params.is_empty() {
                    apply_param_overrides(&mut parameters, params);
                }
                self.description()
            }
            None => self.description(),
        };
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": description,
                "parameters": parameters,
            }
        })
    }
}

/// Fold per-parameter overrides into a tool's `parameters` JSON Schema. For
/// each `param_name → patch`, the patch is deep-merged into
/// `parameters.properties.<param_name>`; if that property doesn't exist it is
/// inserted. Only the named fields on the named parameters change — everything
/// else (other parameters, `required`, top-level `type`) is preserved.
fn apply_param_overrides(
    parameters: &mut serde_json::Value,
    overrides: &HashMap<String, serde_json::Map<String, serde_json::Value>>,
) {
    use serde_json::Value;
    let Some(obj) = parameters.as_object_mut() else {
        return;
    };
    // Ensure `properties` is an object we can mutate.
    if !obj.contains_key("properties") {
        obj.insert("properties".to_string(), Value::Object(Default::default()));
    }
    let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for (param, patch) in overrides {
        let entry = props
            .entry(param.clone())
            .or_insert_with(|| Value::Object(Default::default()));
        let patch_value = Value::Object(patch.clone());
        deep_merge_json(entry, &patch_value);
    }
}

/// What a tool call acts on, so the operation-scope gate can match it against
/// the agent's granted scope. Tools report this via [`Tool::scope_target`];
/// each variant corresponds to one dimension an [`OperationScope`] can
/// constrain. [`ScopeTarget::Unspecified`] is the default for tools with no
/// locatable target and is admitted without a scope check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeTarget {
    /// A filesystem path the tool writes or reads (e.g. `write_file`, `edit_file`).
    /// Checked against the scope's granted directory prefixes.
    Path(std::path::PathBuf),
    /// A shell command string (e.g. `bash`). Checked against the scope's command
    /// allowlist, when one is set.
    Command(String),
    /// The tool declares no locatable target (e.g. `grep`, `list_dir`). Admitted
    /// by the scope gate without a dimension check.
    Unspecified,
}

/// A shell-command allowlist for the [`OperationScope::commands`] dimension.
///
/// Patterns are matched against the *command prefix* of the executed command —
/// the leading program plus any leading env-var assignments (`KEY=val ...`).
/// Matching is by token prefix: `git` admits `git status` and `git diff`; `git`
/// does *not* admit `gitk`. An empty allowlist admits nothing. `*` is a literal
/// pattern meaning "any command" (useful to express "commands unrestricted").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandScope {
    /// Canonicalized command prefixes that are permitted, e.g. `["git", "cargo", "rg"]`.
    /// Order is irrelevant; matched by membership.
    allowed: Vec<String>,
}

impl CommandScope {
    /// Build from an explicit list of allowed command prefixes.
    pub fn new(allowed: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
        }
    }

    /// An empty allowlist — admits nothing. Distinct from "no command
    /// constraint at all" (which is `OperationScope::commands == None`).
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether `command` is permitted under this allowlist. The first whitespace
    /// token is the program name; an `A=B` prefix (env-var assignment) is
    /// skipped so `PYTHONPATH=/x python3 script.py` matches a `python3` grant.
    /// A literal `"*"` allowlist entry admits any command.
    pub fn allows(&self, command: &str) -> bool {
        if self.allowed.iter().any(|p| p == "*") {
            return true;
        }
        let program = leading_program(command);
        self.allowed.contains(&program)
    }
}

/// Extract the leading program name from a command string, skipping any
/// `KEY=val` env-var assignments that precede it. Returns `""` for empty input.
fn leading_program(command: &str) -> String {
    command
        .split_whitespace()
        .find(|tok| !tok.contains('='))
        .unwrap_or("")
        .to_string()
}

/// Runtime operation boundary for an agent — a **hard capability limit, not a
/// prompt**: calls whose [`ScopeTarget`] falls outside the granted scope are
/// blocked outright. `OperationScope` scopes *where* (paths) and *what*
/// (commands) a tool may touch. A tool with [`ScopeTarget::Unspecified`] (no
/// locatable target, e.g. `read_text`, `grep`) skips the scope gate and the
/// permission broker entirely; a tool with a `Path`/`Command` target is checked
/// against this scope first, then surfaces to the broker for approval. See
/// ADR-0028.
///
/// Each dimension is optional: `None` means "no constraint along this axis"
/// (admit anything for that dimension), not "admit nothing". A dimension set to
/// `Some(CommandScope::none())` does mean "admit no command". This lets a scope
/// say "paths unrestricted but commands limited to git" without coupling the
/// two axes.
///
/// The main agent carries an unconstrained scope (the broker is still the
/// interactive layer inside it); a subagent carries the scope resolved from its
/// profile's `write_paths` and `command_allowlist` grants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperationScope {
    /// Granted write-path prefixes. `None` = paths unconstrained;
    /// `Some(vec![])` = no paths permitted.
    pub paths: Option<Vec<std::path::PathBuf>>,
    /// Granted command prefixes. `None` = commands unconstrained;
    /// `Some(CommandScope::none())` = no commands permitted.
    pub commands: Option<CommandScope>,
}

impl OperationScope {
    /// No constraints at all — the main agent's default. Every target is admitted.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// Whether a call with the given [`ScopeTarget`] is permitted under this
    /// scope. Dispatches on the target variant:
    /// - [`ScopeTarget::Path`] → checked against `paths` (prefix-containment,
    ///   canonicalizing the target's parent so a not-yet-existing file resolves).
    /// - [`ScopeTarget::Command`] → checked against `commands` (prefix allowlist).
    /// - [`ScopeTarget::Unspecified`] → admitted (no locatable target to check).
    ///
    /// A dimension that is `None` (unset) admits everything along that axis.
    pub fn allows(&self, target: &ScopeTarget) -> bool {
        match target {
            ScopeTarget::Unspecified => true,
            ScopeTarget::Path(p) => match &self.paths {
                None => true,
                Some(dirs) => match resolve_for_check(&p.to_string_lossy()) {
                    Some(target) => dirs.iter().any(|dir| target.starts_with(dir)),
                    None => false,
                },
            },
            ScopeTarget::Command(cmd) => match &self.commands {
                None => true,
                Some(scope) => scope.allows(cmd),
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
    use super::{CommandScope, OperationScope, ScopeTarget, Tool};
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn unrestricted_allows_everything() {
        let scope = OperationScope::unrestricted();
        assert!(scope.allows(&ScopeTarget::Path(PathBuf::from("anywhere/x.rs"))));
        assert!(scope.allows(&ScopeTarget::Command("rm -rf /".to_string())));
        assert!(scope.allows(&ScopeTarget::Unspecified));
    }

    #[test]
    fn scoped_paths_allows_under_granted_dir_and_blocks_outside() {
        // Simulate resolve_operation_scope's output: a canonical dir prefix.
        let cwd = std::env::current_dir().unwrap();
        let granted: PathBuf = cwd.join("output");
        let scope = OperationScope {
            paths: Some(vec![granted.clone()]),
            commands: None,
        };

        // A new file under the granted dir resolves to granted/file and is allowed,
        // even though neither the dir nor the file exists yet.
        assert!(scope.allows(&ScopeTarget::Path(granted.join("result.md"))));
        // A path outside the granted dir is blocked.
        assert!(!scope.allows(&ScopeTarget::Path(cwd.join("src/main.rs"))));
    }

    #[test]
    fn command_scope_allows_listed_program_and_blocks_others() {
        let scope = CommandScope::new(["git".to_string(), "cargo".to_string()]);
        assert!(scope.allows("git status"));
        assert!(scope.allows("cargo build"));
        assert!(!scope.allows("rm -rf /"));
        // gitk must NOT match a `git` grant (token-prefix, not string-prefix).
        assert!(!scope.allows("gitk"));
    }

    #[test]
    fn command_scope_skips_env_var_assignments() {
        let scope = CommandScope::new(["python3".to_string()]);
        assert!(scope.allows("PYTHONPATH=/x python3 script.py"));
        assert!(!scope.allows("PYTHONPATH=/x ruby script.rb"));
    }

    #[test]
    fn command_scope_wildcard_admits_anything() {
        let scope = CommandScope::new(["*".to_string()]);
        assert!(scope.allows("rm -rf /"));
        assert!(scope.allows("git status"));
    }

    #[test]
    fn operation_scope_paths_none_admits_any_path() {
        let scope = OperationScope {
            paths: None,
            commands: None,
        };
        assert!(scope.allows(&ScopeTarget::Path(PathBuf::from("/etc/passwd"))));
    }

    #[test]
    fn operation_scope_commands_constrained_but_paths_open() {
        let scope = OperationScope {
            paths: None,
            commands: Some(CommandScope::new(["git".to_string()])),
        };
        // Paths open, commands limited to git.
        assert!(scope.allows(&ScopeTarget::Path(PathBuf::from("/anywhere"))));
        assert!(scope.allows(&ScopeTarget::Command("git push".to_string())));
        assert!(!scope.allows(&ScopeTarget::Command("rm -rf /".to_string())));
    }

    #[test]
    fn leading_program_handles_empty_and_whitespace() {
        assert_eq!(super::leading_program(""), "");
        assert_eq!(super::leading_program("   "), "");
        assert_eq!(super::leading_program("git"), "git");
        assert_eq!(super::leading_program("  git   status "), "git");
    }

    /// A minimal [`Tool`] stand-in so the schema-with-overrides tests can run
    /// without pulling in the whole tool crate.
    struct DummyTool {
        name: &'static str,
        desc: &'static str,
    }

    #[async_trait::async_trait]
    impl super::Tool for DummyTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    fn desc_of(schema: &serde_json::Value) -> &str {
        schema["function"]["description"].as_str().unwrap_or("")
    }

    #[test]
    fn schema_uses_built_in_description_when_no_override() {
        let tool = DummyTool {
            name: "read_text",
            desc: "built-in description",
        };
        let empty = super::ToolOverrides::new();
        let schema = tool.to_openai_function_with(&empty);
        assert_eq!(desc_of(&schema), "built-in description");
        // Plain method agrees (the override path is a strict superset).
        assert_eq!(desc_of(&tool.to_openai_function()), "built-in description");
    }

    #[test]
    fn override_replaces_description_for_named_tool_only() {
        let tool = DummyTool {
            name: "read_text",
            desc: "built-in description",
        };
        let mut overrides = super::ToolOverrides::new();
        overrides.insert(
            "read_text".to_string(),
            super::ToolOverride {
                description: Some("custom model-specific wording".to_string()),
                params: HashMap::new(),
            },
        );
        overrides.insert(
            "other_tool".to_string(),
            super::ToolOverride {
                description: Some("unused".to_string()),
                params: HashMap::new(),
            },
        );
        let schema = tool.to_openai_function_with(&overrides);
        assert_eq!(desc_of(&schema), "custom model-specific wording");
        // Name is never touched by an override.
        assert_eq!(schema["function"]["name"], "read_text");
    }

    #[test]
    fn override_miss_leaves_built_in_description_intact() {
        let tool = DummyTool {
            name: "bash",
            desc: "built-in description",
        };
        let mut overrides = super::ToolOverrides::new();
        overrides.insert(
            "read_text".to_string(),
            super::ToolOverride {
                description: Some("irrelevant".to_string()),
                params: HashMap::new(),
            },
        );
        let schema = tool.to_openai_function_with(&overrides);
        assert_eq!(desc_of(&schema), "built-in description");
    }

    // --- parameter deep-merge ---

    /// A richer Tool with real `properties` so parameter merging is testable.
    struct ParamTool;
    #[async_trait::async_trait]
    impl super::Tool for ParamTool {
        fn name(&self) -> &str {
            "read_text"
        }
        fn description(&self) -> &str {
            "built-in"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "a file path" },
                    "limit": { "type": "integer", "description": "max lines" }
                },
                "required": ["path"]
            })
        }
        async fn call(&self, _: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    #[test]
    fn param_override_deep_merges_named_field_and_preserves_the_rest() {
        let mut overrides = super::ToolOverrides::new();
        let mut limit_patch = serde_json::Map::new();
        limit_patch.insert("description".to_string(), "不得低于 10".into());
        limit_patch.insert("minimum".to_string(), 10.into());
        let mut params = HashMap::new();
        params.insert("limit".to_string(), limit_patch);
        overrides.insert(
            "read_text".to_string(),
            super::ToolOverride {
                description: None,
                params,
            },
        );

        let schema = ParamTool.to_openai_function_with(&overrides);
        let limit = &schema["function"]["parameters"]["properties"]["limit"];
        // Patched fields applied…
        assert_eq!(limit["description"], "不得低于 10");
        assert_eq!(limit["minimum"], 10);
        // …but the pre-existing type on `limit` is preserved (not overwritten).
        assert_eq!(limit["type"], "integer");
        // Other parameters and top-level keys are untouched.
        assert_eq!(
            schema["function"]["parameters"]["properties"]["path"]["description"],
            "a file path"
        );
        assert_eq!(
            schema["function"]["parameters"]["required"],
            serde_json::json!(["path"])
        );
    }

    #[test]
    fn param_override_inserts_into_a_property_that_did_not_exist() {
        let mut overrides = super::ToolOverrides::new();
        let mut patch = serde_json::Map::new();
        patch.insert("type".to_string(), "boolean".into());
        let mut params = HashMap::new();
        params.insert("verbose".to_string(), patch);
        overrides.insert(
            "read_text".to_string(),
            super::ToolOverride {
                description: None,
                params,
            },
        );
        let schema = ParamTool.to_openai_function_with(&overrides);
        assert_eq!(
            schema["function"]["parameters"]["properties"]["verbose"]["type"],
            "boolean"
        );
        // Original properties survive.
        assert!(schema["function"]["parameters"]["properties"]["path"].is_object());
    }

    #[test]
    fn deep_merge_json_recurses_into_nested_objects() {
        use serde_json::json;
        let mut target = json!({
            "a": { "x": 1, "y": 2 },
            "b": "keep"
        });
        let patch = json!({
            "a": { "y": 99, "z": 3 }
        });
        super::deep_merge_json(&mut target, &patch);
        assert_eq!(target["a"]["x"], 1); // preserved
        assert_eq!(target["a"]["y"], 99); // overwritten
        assert_eq!(target["a"]["z"], 3); // inserted
        assert_eq!(target["b"], "keep"); // untouched
    }
}
