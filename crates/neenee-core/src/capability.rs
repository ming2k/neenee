//! Foundational capability traits: how the harness talks to a model
//! ([`Provider`]) and to tools ([`Tool`]), the stream events a provider emits
//! ([`ProviderStreamEvent`]), and the mid-turn model-context projection hook
//! ([`ContextProjectionGate`]).

use crate::pursuits::TokenUsage;
use crate::tool_output::StdinPolicy;
use crate::{EnvoyEvent, Message, ToolOutput, ToolStream};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Per-model (and per-envoy-profile) variant selection: a map from a
/// capability name (a [`Tool::name`]) to the [`Tool::variant`] id chosen for
/// it. When the agent resolves its toolset for the active model, a capability
/// listed here is realized by its named variant; capabilities absent from the
/// map fall back to their default variant. This is how one logical toolset can
/// hand different models a genuinely different *implementation* of a tool
/// (different description, schema, and behaviour) rather than a re-worded copy
/// of a single impl.
///
/// Configured per model id under `[tool_variants."<model-id>"]` in
/// `config.toml`; the agent selects the map matching `Provider::model()`.
/// Envoy profiles carry their own static selection (see
/// [`crate::EnvoyProfile`]).
pub type VariantSelection = HashMap<String, String>;

/// A shared empty [`VariantSelection`] map, handy as a default borrow target so
/// callers can always hand out `&VariantSelection` without an `Option`.
pub fn empty_variant_selection() -> &'static VariantSelection {
    static EMPTY: std::sync::LazyLock<VariantSelection> =
        std::sync::LazyLock::new(VariantSelection::new);
    &EMPTY
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
    /// Token usage reported by the provider at the end of a stream (e.g. from
    /// an Anthropic `message_delta` event carrying `usage`). Emitted *in
    /// addition to* the content deltas so the harness can book real
    /// `prompt_tokens` instead of estimating them. Providers that never report
    /// usage simply never emit this variant — the harness then falls back to
    /// the local char-class estimator.
    Usage(TokenUsage),
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

    /// Called by the agent before each turn so the provider can prepare tool
    /// schemas. The agent hands in the already-resolved toolset — exactly one
    /// variant per capability for the active model — so each tool's own
    /// [`Tool::to_openai_function`] is authoritative; there is no per-model
    /// patching at this layer. Default is a no-op for providers that don't
    /// support native function calling.
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

    /// Whether this provider surfaces real token usage from the upstream API.
    ///
    /// The harness uses this (together with [`Provider::take_last_usage`]) to
    /// decide whether a turn's token accounting is **reported** (authoritative)
    /// or **estimated** (local heuristic). The token-source report modal
    /// surfaces this distinction so the user can see which turns are measured
    /// and which are guessed.
    ///
    /// Defaults to `false`; concrete providers override it once they actually
    /// parse usage from their HTTP responses.
    fn usage_supported(&self) -> bool {
        false
    }

    /// Drain and return the usage reported by the **last** `chat` /
    /// `stream_chat_events` call, if the provider reports one.
    ///
    /// The contract is "consume once": a provider that supports usage stashes
    /// the most recent `usage` object internally and hands it out here, then
    /// clears it. The harness calls this right after a turn completes so the
    /// value is always fresh. Returns `None` for providers that don't report
    /// usage (the default), in which case the harness estimates locally.
    fn take_last_usage(&self) -> Option<TokenUsage> {
        None
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

    /// The variant id distinguishing this implementation from other variants of
    /// the same capability ([`name`](Self::name)). A capability with a single
    /// implementation uses the default; multiple variants of one capability
    /// share `name()` and differ only in `variant()`. The variant id never
    /// reaches the model — it is the selection key under
    /// `[tool_variants."<model-id>"]` config and in envoy profiles, by which
    /// a model or profile picks which implementation of a capability it sees.
    fn variant(&self) -> &str {
        "default"
    }

    /// Whether executing this tool may block awaiting a live human decision
    /// (e.g. `ask_user`, an approval-gated mode switch). Non-interactive
    /// execution contexts — envoys spawned for autonomous research — have
    /// no user reachable to answer, so a [`crate::envoy::ToolPolicy`] with
    /// `allow_user_interaction: false` excludes these. See ADR-0011.
    fn requires_user(&self) -> bool {
        false
    }

    /// Whether invoking this tool spawns a nested agent. Envoy profiles
    /// exclude these unconditionally to prevent unbounded recursion — the
    /// outermost dispatch tool (`task`) and wrappers around it
    /// (`verify_plan_execution`) override to `true`. See ADR-0011.
    fn spawns_envoy(&self) -> bool {
        false
    }

    /// Whether this tool only functions on a model that can see images
    /// (vision). A vision-only tool (e.g. `read_image`, which feeds the model
    /// an image part) is useless — or actively misleading — on a text-only
    /// model, which strips image parts before the request hits the wire. This
    /// is a **model-capability requirement**, the symmetric counterpart of
    /// [`requires_user`](Self::requires_user): where that gates on whether a
    /// human is reachable, this gates on whether the model can perceive the
    /// tool's output.
    ///
    /// The pool resolver ([`crate::ToolSet::resolve_for`]) treats it as a
    /// **hard** filter: a variant whose `requires_vision()` a model cannot
    /// satisfy is never selectable for that model — it is simply absent from
    /// the resolved set, so no agent-side override can reinstate it. This is
    /// why model capability limits live on the scope/pool axis, not the soft
    /// override axis.
    fn requires_vision(&self) -> bool {
        false
    }

    /// Whether this tool exercises control over the harness itself (e.g. the
    /// abort/exit escape hatch), as opposed to the workspace/filesystem. This
    /// is orthogonal to [`Tool::scope_target`]: `scope_target` classifies *what
    /// the call touches*, while this classifies *process control*. Envoy
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
    /// that spawn envoys (e.g. `task`) override this to forward child
    /// events while still returning a [`ToolOutput`] (typically [`ToolOutput::Text`]).
    ///
    /// `stdin` is the **execution contract** for the child process's stdin
    /// ([`StdinPolicy`]). It is decided *before* spawn by the agent dispatch
    /// layer (never from the model's arguments) and threaded in here, so a
    /// tool like `bash` can provision `/dev/null`, a pre-filled pipe of human
    /// or model-supplied bytes, etc. The default [`StdinPolicy::Closed`]
    /// keeps tools that ignore stdin correct: a child that blocks on
    /// `read(stdin)` gets instant EOF instead of hanging silently until the
    /// wall-clock timeout.
    async fn call_structured_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(EnvoyEvent) + Send + 'a>,
        _on_stream: &mut (dyn FnMut(ToolStream) + Send + 'a),
        _stdin: StdinPolicy,
    ) -> Result<ToolOutput, String> {
        let _ = _stdin;
        self.call_structured(arguments).await
    }

    /// Execute the tool while optionally emitting events (e.g. envoy steps).
    ///
    /// The default implementation simply calls `call()` and emits no events.
    /// Tools that spawn envoys can override this to stream child events back
    /// to the parent harness.
    async fn call_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(EnvoyEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.call(arguments).await
    }

    /// Generate an OpenAI-compatible function schema for this tool. This is the
    /// authoritative schema for the variant; per-model differences are expressed
    /// by selecting a different variant, not by patching this output.
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
/// interactive layer inside it); an envoy carries the scope resolved from its
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

    /// A minimal [`Tool`] stand-in so the schema tests can run without pulling
    /// in the whole tool crate.
    struct DummyTool {
        name: &'static str,
        variant: &'static str,
        desc: &'static str,
    }

    #[async_trait::async_trait]
    impl super::Tool for DummyTool {
        fn name(&self) -> &str {
            self.name
        }
        fn variant(&self) -> &str {
            self.variant
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
    fn variant_defaults_to_default() {
        let tool = DummyTool {
            name: "read_text",
            variant: "default",
            desc: "built-in",
        };
        assert_eq!(tool.variant(), "default");
    }

    #[test]
    fn function_schema_uses_the_variant_own_description() {
        // A variant's own description is authoritative: the function schema
        // carries it verbatim, keyed by the shared capability name.
        let terse = DummyTool {
            name: "read_text",
            variant: "terse",
            desc: "terse wording",
        };
        let schema = terse.to_openai_function();
        assert_eq!(schema["function"]["name"], "read_text");
        assert_eq!(desc_of(&schema), "terse wording");
    }
}
