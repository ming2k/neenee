use super::*;

use crate::permission_store::PermissionRule;
use futures::future::BoxFuture;

/// Mid-turn save-point closure installed by orchestration (ADR-0035).
///
/// Invoked at each tool-round boundary with the *current full* turn history.
/// The implementation diffs against its own durable baseline and appends only
/// the new tail to the session event log (see `SessionStore::append_turn`).
/// Errors are surfaced back to the ReAct loop, which treats a persist failure
/// as a turn-ending error (better to stop than to keep mutating state that may
/// not be recoverable).
pub(crate) type RoundPersistFn =
    Arc<dyn Fn(&[Message]) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// Who an [`Agent`] is and what it is for. This crate is identity-agnostic: it
/// does not hardcode "neenee" or "coding". The embedding (the CLI, a future
/// frontend) supplies the fields so the same engine can be repurposed as a
/// different persona or for a different mission (research, ops, writing) by
/// passing different values. Everything else in the system prompt (tone,
/// todo/ask_user guidance) is mission-neutral and stays here.
///
/// The three fields compose the opening line:
/// - [`AgentIdentity::name`] — what the agent is called ("neenee" for this
///   project; swap to repurpose the engine under a different product).
/// - [`AgentIdentity::mission`] — what the agent is for ("an expert AI coding
///   assistant…" for this CLI; swap for research/ops/etc.).
/// - [`AgentIdentity::persona`] — optional full-text override of the opening.
///   When set, [`AgentIdentity::preamble`] returns it verbatim and ignores
///   `name`/`mission`. Envoys use this to inject their role's full system
///   prompt as the identity.
///
/// [`AgentIdentity::default`] yields empty fields (no preamble — the system
/// prompt opens straight at the tone line); tests use it.
#[derive(Debug, Clone, Default)]
pub struct AgentIdentity {
    /// What this agent is called, e.g. `"neenee"`. Empty means "unnamed".
    pub name: String,
    /// What this agent is for, e.g. `"an expert AI coding assistant with tool
    /// access"`. Empty means "no mission framing".
    pub mission: String,
    /// Optional full-text identity override. When non-empty, `preamble`
    /// returns this verbatim (used by envoys whose identity *is* their
    /// role's full system prompt). None/empty → compose from name + mission.
    pub persona: Option<String>,
}

impl AgentIdentity {
    /// Build a structured identity from a name and a mission.
    pub fn new(name: impl Into<String>, mission: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mission: mission.into(),
            persona: None,
        }
    }

    /// Build an identity whose preamble is a full persona string, ignoring
    /// name/mission composition. Used by envoys: their identity is the
    /// role's complete system prompt.
    pub fn from_persona(persona: impl Into<String>) -> Self {
        Self {
            name: String::new(),
            mission: String::new(),
            persona: Some(persona.into()),
        }
    }

    /// Render the opening system-prompt sentence. A `persona` override returns
    /// it verbatim; otherwise `"You are {name}, {mission}."` when both are set,
    /// `"You are {name}."` / `"You are {mission}."` when one is set, and the
    /// empty string when neither is (tests / identity-less agents).
    pub fn preamble(&self) -> String {
        if let Some(persona) = &self.persona
            && !persona.is_empty()
        {
            return persona.clone();
        }
        match (self.name.is_empty(), self.mission.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!("You are {}.", self.name),
            (true, false) => format!("You are {}.", self.mission),
            (false, false) => format!("You are {}, {}.", self.name, self.mission),
        }
    }
}

#[derive(Default)]
struct AskUserState {
    pending: HashMap<String, oneshot::Sender<Option<UserQuestionReply>>>,
}

/// Parked oneshots for in-flight interactive-input requests (L3.5 β): a
/// `bash` command classified interactive blocks here until the operator's
/// [`InputReply`] arrives (or `None` on cancel/turn-end).
#[derive(Default)]
struct InputState {
    pending: HashMap<String, oneshot::Sender<Option<InputReply>>>,
}

pub struct Agent {
    pub provider: Arc<dyn Provider>,
    /// The full capability set: every tool keyed by capability, with all its
    /// variants. The single source of truth from which the model-visible
    /// [`resolved_tools`](Self::resolved_tools) view is derived for the active
    /// [`variant_selection`](Self::variant_selection).
    pub(crate) toolset: neenee_core::ToolSet,
    /// The active resolved view: exactly one variant per capability, for the
    /// current model's [`variant_selection`](Self::variant_selection). Both advertising
    /// (`visible_tools` → `prepare_tools`) and dispatch (`find` by name) read
    /// this, so re-resolving it on a model/selection switch makes *both* the
    /// schema and the executed implementation track the chosen variant. Held
    /// behind a `RwLock` because it is swapped wholesale on selection change.
    resolved_tools: std::sync::RwLock<Vec<Arc<dyn Tool>>>,
    /// Dynamically refreshable MCP tools, held behind a shared `RwLock` so the
    /// background `McpCatalog` refresh loop can
    /// replace them (reconnect, re-discover) without rebuilding the agent.
    /// `visible_tools` / dispatch / snapshot merge this with `resolved_tools`.
    mcp_tools: Arc<std::sync::RwLock<Vec<Arc<dyn Tool>>>>,
    /// Session-level disabled-tool mask. Names here are hidden from the model
    /// (their schemas are dropped before `prepare_tools`) and rejected at
    /// dispatch, but the tool stays installed so it can be re-enabled without
    /// rebuilding the agent. Toggled from the session modal via
    /// `set_tool_enabled` / `ToggleTool`.
    disabled_tools: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Unified task list, the single source of truth for "what is left to
    /// do." Drives the sticky panel and persists across restarts. Shared
    /// with the `todo` / `todo_update` tools via `TodoToolContext`.
    todos: Arc<std::sync::Mutex<neenee_core::TodoList>>,
    /// Harness turn counter, bumped at the start of every `execute_round`.
    /// Shared with the todo tools so they can stamp
    /// `updated_at_turn` for the TUI stale detector.
    turn_counter: Arc<std::sync::Mutex<u64>>,
    /// In-memory pursuit state: the active [`Pursuit`], the stop-gate armed
    /// flag, and the iteration counter. See [`crate::pursuit_state::PursuitState`].
    pursuit_state: crate::pursuit_state::PursuitState,
    permissions: crate::permission_store::PermissionStore,
    ask_user: std::sync::Mutex<AskUserState>,
    /// Parked interactive-input requests (L3.5 β). Mirrors `ask_user`.
    input: std::sync::Mutex<InputState>,
    pub(crate) skills_registry: skills::SkillRegistry,
    thread_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Context-pressure threshold (in tokens) above which the harness asks the
    /// [`ContextProjectionGate`] to project the model-visible window between
    /// tool rounds. `0` disables mid-turn projection. Derived from the active
    /// model's context window.
    context_prune_threshold_tokens: Arc<std::sync::Mutex<usize>>,
    /// Optional mid-turn model-context projection gate.
    context_projection_gate: Arc<std::sync::Mutex<Option<Arc<dyn ContextProjectionGate>>>>,
    /// Opt-in hard-stop budget (ADR-0018): abort a turn after this many total
    /// tool rounds. Seeded from `Config::agent.hard_stop_turns` (default `0`
    /// = uncapped, matching ADR-0009) and mutated at runtime via
    /// `set_hard_stop_turns`. This is the sole execution cap; session review
    /// is on-demand (`/review`) and never aborts a turn.
    hard_stop_turns: Arc<std::sync::Mutex<usize>>,
    /// Whether the deterministic read-loop guard ([`crate::loop_guard`]) may
    /// inject its anti-anchoring nudge, and the tunable thresholds it uses.
    /// Default **disabled** ([`neenee_core::NudgeConfig::default`]);
    /// seeded from `[principal.nudge]` in `config.toml` and flipped to
    /// [`NudgeConfig::disabled`] for envoys and the review diagnostic via
    /// `set_nudge_config`. Held behind an `Arc<RwLock>` so a runtime update
    /// from the `/config` modal (`set_nudge_config`) replaces both the master
    /// switch and the thresholds atomically; the round-boundary path reads
    /// `enabled` and the per-turn guard reads the thresholds when it is
    /// constructed (`TurnState::guards_default`).
    nudge_config: Arc<std::sync::RwLock<neenee_core::NudgeConfig>>,
    /// Whether the model may supply stdin bytes for a `bash` call it emits
    /// (the opt-in automatic-flow path, L3.5 α). Default `false`; seeded from
    /// `[principal] allow_model_stdin`. Lock-free so the dispatch site reads
    /// it without contention. When `false` the bash schema exposes no `stdin`
    /// parameter (structurally unreachable from the model); when `true` it
    /// does, and a model-supplied `stdin` is threaded as
    /// [`StdinPolicy::Prefilled`].
    allow_model_stdin: Arc<std::sync::atomic::AtomicBool>,
    /// Registered review dimensions evaluated by the on-demand diagnostic
    /// envoy (`/review`). Defaults to [`crate::default_reviews`] (looping);
    /// empty on envoys (which have no `/review` path).
    reviews: Vec<Arc<dyn SessionReview>>,
    /// Runtime operation boundary for this agent (ADR-0028). The main agent is
    /// unrestricted ([`neenee_core::OperationScope::unrestricted`]); an envoy
    /// carries the scope resolved from its profile's `write_paths` and
    /// `command_allowlist` grants. Enforced at the `execute_tool` funnel for
    /// every admitted tool whose [`neenee_core::ScopeTarget`] falls outside the
    /// granted scope, before the permission broker — a hard boundary, not a
    /// prompt.
    operation_scope: std::sync::Mutex<neenee_core::OperationScope>,
    /// Lifecycle event hooks (ADR-0025). Installed once at startup from the
    /// `[hooks]` config by the CLI; empty by default (envoys, tests). Read
    /// at the PreToolUse / PostToolUse / Stop insertion points. Held as a
    /// swappable `Arc` behind a `Mutex` so [`Agent::set_hooks`] can replace the
    /// whole registry without the insertion points holding the lock across the
    /// async `fire` — they clone the `Arc` and drop the guard first.
    hooks: crate::hook_runner::HookRunner,
    /// Inbound steering inbox — the down-direction of full-duplex (ADR-0029).
    /// `None` for agents that were never given a handle (the top-level agent
    /// driven directly by the harness, legacy tests); lazily created by
    /// [`Agent::install_inbox`], which a spawned envoy's dispatcher
    /// (`EnvoyTool`) calls so the parent can steer it mid-turn. The driver loop
    /// `take`s the receiver at turn entry and drains it at every tool-round
    /// boundary (see [`Agent::drain_inbox`]). Carries only the
    /// "new-input / control" class ([`AgentOp`]); the request/reply class
    /// (permission / ask_user) bypasses this queue and resolves the parked
    /// oneshot directly via `reply_permission` / `reply_user_question`, since a
    /// reply must unblock a tool parked mid-turn and cannot wait for the loop.
    inbox_tx: std::sync::Mutex<Option<mpsc::UnboundedSender<AgentOp>>>,
    inbox_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<AgentOp>>>,
    /// Who this agent is and what it is for. The single string the system
    /// prompt opens with — supplied by the *embedding* (e.g. the CLI), so this
    /// crate stays identity-agnostic and can be reused by frontends that are
    /// not "neenee". See [`AgentIdentity`].
    pub(crate) identity: AgentIdentity,
    /// Optional mid-turn save point invoked at every tool-round boundary
    /// (ADR-0035). The embedding (orchestration) installs a closure that
    /// durably appends the round's new messages to the session log so a crash
    /// after a side-effecting tool call leaves the transcript in sync with the
    /// filesystem instead of rewinding to the previous turn. `None` for
    /// envoys, the review diagnostic, and tests — they have no session of
    /// their own to persist, so the round boundary is a plain no-op there.
    round_persist: std::sync::Mutex<Option<RoundPersistFn>>,
    /// Declarative prompt registry (ADR-0039). Holds one [`PromptSection`] per
    /// injection path, keyed by id. Seeded with the default system-channel
    /// sections in [`Agent::new`] via [`crate::prompt::default_prompt_registry`];
    /// the system message is rebuilt each round by composing the active
    /// sections in rank order. User-channel sections are added in later
    /// migration stages.
    pub(crate) prompt_registry: crate::PromptRegistry,
    /// Per-model tool-variant selection (the **override** axis) for the
    /// *current* model: a `capability → variant_id` map. Seeded from
    /// `[tool_variants."<model-id>"]` config via
    /// [`Agent::set_variant_selection`] and re-seeded on model switch so the
    /// resolved toolset always tracks the live model. Held behind an `Arc` so a
    /// spawned envoy — which is an agent on the *same* model — can inherit
    /// the same overrides by sharing this handle (see
    /// [`Agent::variant_selection_handle`]); the agent decides scope, the
    /// model decides variant.
    variant_selection: Arc<std::sync::Mutex<neenee_core::VariantSelection>>,
    /// This agent's **identity-side selection** of the pool (the agent half of
    /// the two-selector model): the capability scope it admits plus any variant
    /// pins it forces. The principal agent is
    /// [`ToolSelection::unrestricted`](neenee_core::ToolSelection::unrestricted)
    /// — every capability, model-chosen variants. A scoped agent (or a future
    /// role-bound principal) narrows this. Composed with the live model's
    /// selection by [`neenee_core::ToolSet::resolve_for`] every time the toolset
    /// is re-resolved: scope by intersection, variants by agent-over-model
    /// precedence, model capability limits applied hard.
    agent_selection: std::sync::Mutex<neenee_core::ToolSelection>,
    /// Token-source accounting: running tally of how many tokens each
    /// provider+model reported authoritatively (upstream `usage`) vs. how many
    /// were filled in by the local estimator. Shared with the TUI so the
    /// token-source report modal renders live. `None` for envoys/tests that
    /// don't surface the report.
    token_ledger: std::sync::Mutex<Option<Arc<neenee_core::TokenSourceLedger>>>,
}

/// Capability handle for steering a running agent from the outside — the
/// parent's down-direction of full-duplex (ADR-0029). Cheap to clone (one
/// `Weak` + one `mpsc::Sender`); obtained from [`Agent::install_inbox`] on an
/// `Arc<Agent>` (a spawned envoy) and typically lodged in a
/// [`crate::envoy_tool::EnvoyRegistry`] keyed by the parent tool-call id so
/// the harness can look it up when a request surfaces.
///
/// Two classes of operation, deliberately split:
///
/// - **Steering** ([`AgentOp`], via [`EnvoyHandle::submit`]): inject a new
///   user message, a hidden inter-agent note, or interrupt/shutdown. Routed
///   through the agent's inbox and applied at the next tool-round boundary —
///   safe to defer because nothing is blocked on it.
/// - **Request/reply** ([`EnvoyHandle::reply_permission`] /
///   [`EnvoyHandle::reply_user_question`]): resolve a permission broker or
///   `ask_user` oneshot the envoy is parked on **right now**, mid-tool.
///   These bypass the inbox and call the agent's shared-state resolvers
///   directly — a queued reply would deadlock the parked tool.
///
/// The `Weak<Agent>` means the handle observes the agent's lifetime: once the
/// envoy's turn ends and the dispatcher drops its `Arc`, every method
/// returns `false` / `None` instead of erroring, so a late reply from the UI
/// after the envoy finished degrades gracefully.
#[derive(Clone)]
pub struct EnvoyHandle {
    weak: std::sync::Weak<Agent>,
    ops: mpsc::UnboundedSender<AgentOp>,
}

impl EnvoyHandle {
    /// Submit a steering [`AgentOp`] into the agent's inbox. Returns `false`
    /// if the agent has been dropped (receiver gone) — the op is discarded.
    pub fn submit(&self, op: AgentOp) -> bool {
        self.ops.send(op).is_ok()
    }

    /// Resolve a permission broker request the envoy is parked on. Returns
    /// `false` if the agent was dropped or no matching pending request exists.
    /// This is the down-direction counterpart to an up-going
    /// [`AgentEvent::PermissionRequest`] / [`EnvoyEvent::PermissionRequest`].
    pub fn reply_permission(&self, request_id: &str, decision: PermissionDecision) -> bool {
        if let Some(agent) = self.weak.upgrade() {
            agent.reply_permission(request_id, decision)
        } else {
            false
        }
    }

    /// Resolve an `ask_user` request the envoy is parked on. Returns
    /// `false` if the agent was dropped or no matching pending request exists.
    /// Down-direction counterpart to an up-going
    /// [`AgentEvent::UserQuestionRequest`] / [`EnvoyEvent::UserQuestionRequest`].
    pub fn reply_user_question(&self, request_id: &str, answers: Vec<Vec<String>>) -> bool {
        if let Some(agent) = self.weak.upgrade() {
            agent.reply_user_question(request_id, answers)
        } else {
            false
        }
    }

    /// Resolve an interactive-input request the envoy's `bash` is parked on
    /// (L3.5 β). Down-direction counterpart to an up-going
    /// [`AgentEvent::InputRequest`] / [`EnvoyEvent::InputRequest`].
    pub fn reply_input(&self, request_id: &str, text: String) -> bool {
        if let Some(agent) = self.weak.upgrade() {
            agent.reply_input(request_id, text)
        } else {
            false
        }
    }

    /// Whether the underlying agent is still alive (its dispatcher still holds
    /// the `Arc`). Lets a caller drop a stale handle instead of no-op-ing.
    pub fn is_alive(&self) -> bool {
        self.weak.upgrade().is_some()
    }
}

/// Mutable bookkeeping threaded through a single turn's tool-dispatch rounds.
#[derive(Default)]
pub(crate) struct TurnState {
    token_usage: TokenUsage,
    /// Consecutive rounds whose tool calls were all `Read`-tier. Surfaced to
    /// user-configured round hooks (`HookEvent::Turn { consecutive_readonly }`)
    /// so a hook can act on "exploration without progress". Reset to 0 by any
    /// round containing an `Execute`/`Write` call.
    pub(crate) consecutive_readonly_rounds: u32,
    /// The turn-guard registry: holds one or more `RoundGuard`s (e.g.
    /// `ReadLoopGuard`) and the tool-call data for the round just dispatched.
    /// Per-turn: lives and dies with this `TurnState` so loop state never
    /// crosses turns.
    pub(crate) guards: crate::loop_guard::RoundGuardState,
}

impl TurnState {
    /// Build a fresh per-turn guard state with the standard guard set, tuned
    /// by `config`. Whether the guard is *enabled* (allowed to inject) is
    /// controlled by `config.enabled`, checked at the round boundary in
    /// [`Agent::apply_guard_actions`] — so the guard state is always present
    /// even when disabled (it just never fires). Per-turn: lives and dies
    /// with this `TurnState` so loop state never crosses turns.
    fn guards_default(config: neenee_core::NudgeConfig) -> crate::loop_guard::RoundGuardState {
        let mut registry = crate::loop_guard::GuardRegistry::new();
        registry.register(Box::new(crate::loop_guard::ReadLoopGuard::new(config)));
        crate::loop_guard::RoundGuardState::new(registry)
    }
}

impl Agent {
    /// Construct an agent from a flat tool list. The tools are grouped into a
    /// [`neenee_core::ToolSet`] (one capability per [`Tool::name`], one variant
    /// per [`Tool::variant`]) — the common case for a single-variant toolset or
    /// an already-resolved envoy toolset. Use [`Agent::from_toolset`] to
    /// preserve a multi-variant set so per-model variant selection can switch
    /// between variants at runtime.
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        skills_registry: skills::SkillRegistry,
        identity: AgentIdentity,
    ) -> Self {
        Self::from_toolset(
            provider,
            neenee_core::ToolSet::from_tools(tools),
            skills_registry,
            identity,
        )
    }

    /// Construct an agent from a full [`neenee_core::ToolSet`], preserving every
    /// capability's variants so [`Agent::set_variant_selection`] can swap the
    /// model-visible variant at runtime.
    pub fn from_toolset(
        provider: Arc<dyn Provider>,
        toolset: neenee_core::ToolSet,
        skills_registry: skills::SkillRegistry,
        identity: AgentIdentity,
    ) -> Self {
        let pursuit_state = crate::pursuit_state::PursuitState::new();
        let thread_id = Arc::new(std::sync::Mutex::new(None));

        let mut toolset = toolset;

        // The unified task list shares its cell + turn counter with the
        // todo tools. An ad-hoc task edit (todo / todo_update) moves the
        // same shared list.
        let turn_counter = Arc::new(std::sync::Mutex::new(0u64));
        let todos = Arc::new(std::sync::Mutex::new(neenee_core::TodoList::default()));
        let todo_context =
            neenee_core::TodoToolContext::new(Arc::clone(&todos), Arc::clone(&turn_counter));
        toolset.insert(Arc::new(crate::todo_tools::TodoWriteTool::new(
            todo_context.clone(),
        )));
        toolset.insert(Arc::new(crate::todo_tools::TodoUpdateTool::new(
            todo_context,
        )));

        // Seed the model-visible view by resolving the pool for the live model
        // with no role restriction and no model variant overrides yet: the
        // principal's identity selection (unrestricted) composed with the
        // model's capability limits. `set_variant_selection` re-resolves once
        // the model's `[tool_variants]` selection is known and on every switch.
        let agent_selection = neenee_core::ToolSelection::unrestricted();
        let seed_model = neenee_core::resolve_model(&provider.model());
        let resolved_tools = std::sync::RwLock::new(toolset.resolve_for(
            &seed_model,
            &agent_selection,
            &neenee_core::ToolSelection::unrestricted(),
        ));

        Self {
            provider,
            toolset,
            resolved_tools,
            mcp_tools: Arc::new(std::sync::RwLock::new(Vec::new())),
            disabled_tools: Arc::new(std::sync::Mutex::new(HashSet::new())),
            todos,
            turn_counter,
            pursuit_state,
            permissions: crate::permission_store::PermissionStore::new(),
            ask_user: std::sync::Mutex::new(AskUserState::default()),
            input: std::sync::Mutex::new(InputState::default()),
            skills_registry,
            thread_id,
            context_prune_threshold_tokens: Arc::new(std::sync::Mutex::new(0)),
            context_projection_gate: Arc::new(std::sync::Mutex::new(None)),
            hard_stop_turns: Arc::new(std::sync::Mutex::new(0)),
            nudge_config: Arc::new(std::sync::RwLock::new(neenee_core::NudgeConfig::default())),
            allow_model_stdin: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reviews: crate::default_reviews(),
            operation_scope: std::sync::Mutex::new(neenee_core::OperationScope::unrestricted()),
            hooks: crate::hook_runner::HookRunner::new(),
            inbox_tx: std::sync::Mutex::new(None),
            inbox_rx: std::sync::Mutex::new(None),
            identity,
            round_persist: std::sync::Mutex::new(None),
            prompt_registry: crate::prompt::default_prompt_registry(),
            variant_selection: Arc::new(
                std::sync::Mutex::new(neenee_core::VariantSelection::new()),
            ),
            agent_selection: std::sync::Mutex::new(agent_selection),
            token_ledger: std::sync::Mutex::new(None),
        }
    }

    /// Context-pressure threshold (in tokens) for mid-turn relief. `0` (the
    /// default) disables the mid-turn [`ContextProjectionGate`]. Re-seed on provider
    /// switch so the threshold tracks the new model's context window.
    pub fn set_context_prune_threshold(&self, budget_tokens: usize) {
        *self
            .context_prune_threshold_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = budget_tokens;
    }

    /// Replace the per-model tool-variant selection and re-resolve the
    /// model-visible toolset to match. Seeded from `[tool_variants."<model-id>"]`
    /// config and re-applied on model switch so the resolved variants — and the
    /// live model's hard capability limits (e.g. vision) — always track the
    /// live model. An empty map (the default) realizes every capability with its
    /// model-chosen / default variant.
    pub fn set_variant_selection(&self, selection: neenee_core::VariantSelection) {
        self.reresolve_tools(&selection);
        *self
            .variant_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = selection;
    }

    /// Replace this agent's identity-side selection (capability scope + variant
    /// pins) and re-resolve the model-visible toolset. The principal is
    /// unrestricted by default; this narrows it (e.g. confining a role-bound
    /// principal to a capability subset). The current per-model variant
    /// selection is preserved and re-composed.
    pub fn set_agent_selection(&self, selection: neenee_core::ToolSelection) {
        *self
            .agent_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = selection;
        let model_variants = self
            .variant_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.reresolve_tools(&model_variants);
    }

    /// Re-resolve [`resolved_tools`](Self::resolved_tools) from the pool for the
    /// live model, composing this agent's identity selection with the model's
    /// selection (`model_variants` overrides + the model's hard capability
    /// limits). The single choke point through which both the principal seed and
    /// every model/selection switch flow, so the schema sent to the provider and
    /// the dispatch table always reflect `agent_scope ∩ model_caps`.
    fn reresolve_tools(&self, model_variants: &neenee_core::VariantSelection) {
        let model = neenee_core::resolve_model(&self.provider.model());
        let agent_selection = self
            .agent_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let model_selection =
            neenee_core::ToolSelection::unrestricted().with_variants(model_variants.clone());
        *self
            .resolved_tools
            .write()
            .unwrap_or_else(|e| e.into_inner()) =
            self.toolset
                .resolve_for(&model, &agent_selection, &model_selection);
    }

    /// The current model-visible toolset: exactly one variant per capability for
    /// the active selection. A snapshot clone for external readers (e.g. a
    /// envoy dispatch that scopes this resolved list down to its profile).
    pub fn installed_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.resolved_tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Dev-only dry run: rebuild the head system message and auto-load any
    /// skills mentioned in the latest visible user turn against a borrowed
    /// message list, exactly as the next turn would (`prepare_turn_messages`),
    /// but with no provider call and no mutation of live turn history. Powers
    /// the `/debug context` snapshot so it captures the *real* request shape —
    /// including the freshly composed system prompt and injected skills —
    /// rather than a degenerate reconstruction.
    pub fn prepare_turn_messages_debug(&self, messages: &mut Vec<Message>) {
        self.prepare_turn_messages(messages);
    }

    /// A shared handle to this agent's live variant selection (the **override**
    /// axis). Handed to a spawned envoy's dispatch tool so the envoy — an
    /// agent on the same model — resolves its admitted capabilities to the same
    /// variants the parent uses, tracking model switches live. The profile still
    /// owns the orthogonal **scope** axis.
    pub fn variant_selection_handle(&self) -> Arc<std::sync::Mutex<neenee_core::VariantSelection>> {
        Arc::clone(&self.variant_selection)
    }

    /// Replace the prompt registry wholesale. Used by sub-callers that need a
    /// different system-message composition than the default mission-neutral
    /// set — currently the `/review` diagnostic, whose reviewer envoy gets
    /// a persona + dimensions + JSON-contract registry (ADR-0039 stage 6) so
    /// `ensure_system_prompt` rebuilds the review prompt correctly each round
    /// instead of clobbering a pre-seeded system message.
    pub(crate) fn set_prompt_registry(&mut self, registry: crate::PromptRegistry) {
        self.prompt_registry = registry;
    }

    /// Override the opt-in hard-stop budget. Mirrors `[agent] hard_stop_turns`
    /// in `config.toml` but can be flipped at runtime. `0` (the default) leaves
    /// the turn uncapped, matching ADR-0009. The reviewer envoy gets a
    /// tight non-zero bound so a runaway diagnostic cannot loop.
    pub fn set_hard_stop_turns(&self, rounds: usize) {
        *self
            .hard_stop_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = rounds;
    }

    /// Current hard-stop budget. Read by the `/hard-stop` slash command (if
    /// present) and by `check_hard_stop` each round.
    pub fn get_hard_stop_turns(&self) -> usize {
        *self
            .hard_stop_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The review dimensions effective for this agent: its registered set, or
    /// the built-in defaults ([`crate::default_reviews`]) when none are
    /// registered. Centralizes the "empty → default" fallback so the runner in
    /// `session_review` does not touch private fields.
    pub(crate) fn effective_reviews(&self) -> Vec<Arc<dyn SessionReview>> {
        if self.reviews.is_empty() {
            crate::default_reviews()
        } else {
            self.reviews.to_vec()
        }
    }

    /// Replace the live nudge configuration atomically. The next turn
    /// reconstructs its per-turn guard from the new thresholds (the current
    /// turn, if any, keeps its already-built guard state — per-turn state is
    /// not retroactively patched). The `enabled` flag is read at every round
    /// boundary in `Self::apply_guard_actions`, so a mid-turn toggle takes
    /// effect on the very next round.
    ///
    /// Wired from `[principal.nudge]` in `config.toml` at startup, mutated at
    /// runtime by the `/config` modal, and forced to
    /// [`NudgeConfig::disabled`] on envoys and the review diagnostic so they
    /// run unobstructed regardless of user settings.
    pub fn set_nudge_config(&self, config: neenee_core::NudgeConfig) {
        *self.nudge_config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    /// Snapshot of the live nudge configuration. The `/config` modal reads
    /// this to render the current values; the round boundary reads
    /// `enabled` to gate `Self::apply_guard_actions`.
    pub fn nudge_config(&self) -> neenee_core::NudgeConfig {
        *self.nudge_config.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the deterministic read-loop guard is currently armed (allowed
    /// to inject). Convenience wrapper over [`Self::nudge_config`] for the
    /// round-boundary fast path.
    pub fn nudge_enabled(&self) -> bool {
        self.nudge_config().enabled
    }

    /// Enable or disable the model-supplied-stdin path for `bash` (L3.5 α).
    /// Mirrors `[principal] allow_model_stdin` in `config.toml`. When off
    /// (the default), the bash schema exposes no `stdin` parameter and a
    /// command needing input either gets it from a human (interactive
    /// classifier → input panel) or fails fast. When on, the model may feed
    /// a command's stdin directly — for unattended/automatic flows.
    pub fn set_allow_model_stdin(&self, enabled: bool) {
        self.allow_model_stdin
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the model may supply stdin for a `bash` call. Read at the
    /// dispatch site to decide the [`StdinPolicy`] and whether the bash schema
    /// exposes a `stdin` parameter.
    pub fn allow_model_stdin(&self) -> bool {
        self.allow_model_stdin
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Install (or clear with `None`) the mid-turn model-context projection gate.
    pub fn set_context_projection_gate(&self, gate: Option<Arc<dyn ContextProjectionGate>>) {
        *self
            .context_projection_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// Install the shared token-source ledger so this agent books each turn's
    /// token counts (reported vs. estimated) into it. The embedding shares the
    /// same `Arc` with the TUI so the token-source report modal reads live.
    /// No-op for envoys/tests that never call this (the ledger stays `None`
    /// and booking is skipped).
    pub fn install_token_ledger(&self, ledger: Arc<neenee_core::TokenSourceLedger>) {
        *self.token_ledger.lock().unwrap_or_else(|e| e.into_inner()) = Some(ledger);
    }

    /// A handle to the token-source ledger, if one was installed. The TUI uses
    /// this to snapshot the report for the modal.
    pub fn token_ledger(&self) -> Option<Arc<neenee_core::TokenSourceLedger>> {
        self.token_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Book one turn's token usage into [`TurnState::token_usage`] and, when a
    /// ledger is installed, into the token-source ledger.
    ///
    /// `streamed_usage` is the usage reported mid-stream (OpenAI
    /// `include_usage` / Anthropic `message_delta`), if any. When absent, we
    /// fall back to [`Provider::take_last_usage`] (the non-streaming path) and
    /// finally to the local char-class estimator.
    ///
    /// This is the single point that decides whether a turn counts as
    /// **reported** (authoritative) or **estimated** (heuristic), and records
    /// that classification so the token-source report modal can render it.
    fn book_turn_usage(
        &self,
        state: &mut TurnState,
        response: &Message,
        streamed_usage: Option<TokenUsage>,
    ) {
        let provider_id = self.provider.provider_id();
        let model = self.provider.model();
        // Prefer the usage the provider reported (streamed, then drained).
        let reported = streamed_usage.or_else(|| self.provider.take_last_usage());
        if let Some(usage) = reported {
            state.token_usage.total_tokens += usage.total_tokens;
            state.token_usage.prompt_tokens += usage.prompt_tokens;
            state.token_usage.completion_tokens += usage.completion_tokens;
            state.token_usage.cache_creation_input_tokens += usage.cache_creation_input_tokens;
            state.token_usage.cache_read_input_tokens += usage.cache_read_input_tokens;
            if let Some(ledger) = self
                .token_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                // Use the round overload so the report can surface the per-turn
                // line item and cache hit-rate; the cache write/read counts are
                // already included in usage.total_tokens (folded in by the
                // provider's usage parser), so they're tracked here as a
                // *breakout*, not added again.
                ledger.record_round(
                    &provider_id,
                    &model,
                    neenee_core::TokenRound {
                        reported: true,
                        prompt_tokens: usage.prompt_tokens,
                        completion_tokens: usage.completion_tokens,
                        total_tokens: usage.total_tokens.max(1),
                        cache_write_tokens: usage.cache_creation_input_tokens,
                        cache_read_tokens: usage.cache_read_input_tokens,
                    },
                );
            }
        } else {
            // No upstream usage: fall back to the local estimator and mark the
            // turn as estimated so the report reflects reality.
            let estimated = pressure::estimate_message_tokens(response);
            state.token_usage.total_tokens += estimated;
            if let Some(ledger) = self
                .token_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                ledger.record(&provider_id, &model, estimated, false);
            }
        }
    }

    /// Install the lifecycle hook registry (ADR-0025). Replaces any prior
    /// registry; intended to be called once at startup after the `[hooks]`
    /// config is parsed. Envoys and tests leave the default empty registry.
    pub fn set_hooks(&self, registry: crate::hooks::HookRegistry) {
        self.hooks.set(registry);
    }

    /// Install the mid-turn save point fired at every tool-round boundary
    /// (ADR-0035). The closure receives the *current full* turn history and
    /// should durably append only the new tail (see
    /// `SessionStore::append_turn`). Called once by orchestration after the
    /// agent is built and the session is open; envoys and the review
    /// diagnostic never call this, so the default `None` keeps their round
    /// boundaries no-ops.
    pub fn set_turn_persist(&self, f: RoundPersistFn) {
        *self.round_persist.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// Fire the mid-turn save point if installed. Returns `Ok(())` when no
    /// closure is set (the envoy / review / test path) so the call site
    /// stays unconditional. Invoked at the round boundary — after a round's
    /// tool results are in `messages` and before the next model request.
    async fn fire_round_persist(&self, messages: &[Message]) -> Result<(), HarnessError> {
        let f = self
            .round_persist
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match f {
            Some(f) => f(messages).await.map_err(|error| {
                HarnessError::Other(format!("could not persist mid-turn round: {error}"))
            }),
            None => Ok(()),
        }
    }

    /// Snapshot the hook registry as a cheap `Arc` clone, so insertion points
    /// fire hooks without holding the swap lock across the async `fire`.
    fn hooks(&self) -> Arc<crate::hooks::HookRegistry> {
        self.hooks.get()
    }

    /// The session id hooks see (the live thread id, if any).
    fn hook_session_id(&self) -> String {
        self.thread_id().unwrap_or_default()
    }

    /// The cwd hooks run under (the persisted project root, if any).
    fn hook_cwd(&self) -> Option<std::path::PathBuf> {
        self.permissions.project_root()
    }

    // --- Public hook entry points (ADR-0025) ---------------------------------
    // The PreToolUse / PostToolUse / Stop insertion points are inline in the
    // loop above (they need local control flow); the lifecycle entry points
    // below are called by the driver / orchestration at the session, turn, and
    // compaction boundaries.

    /// `UserPromptSubmit` gate. Called by `execute_round` before the prompt
    /// enters the transcript: a `Deny` drops it, a `Prepend` prefixes context.
    pub async fn fire_user_prompt_submit(&self, prompt: &str) -> crate::hooks::UserPromptVerdict {
        self.hooks()
            .check_user_prompt_submit(prompt, &self.hook_session_id(), self.hook_cwd().as_deref())
            .await
    }

    /// `PreCompact` observers. Returns any injected context to fold into the
    /// upcoming summarization (ADR-0025).
    pub async fn fire_pre_compact(&self) -> Vec<String> {
        self.hooks()
            .pre_compact(&self.hook_session_id(), self.hook_cwd().as_deref())
            .await
    }

    /// `PostCompact` observers. Informational only.
    pub async fn fire_post_compact(&self) {
        self.hooks()
            .post_compact(&self.hook_session_id(), self.hook_cwd().as_deref())
            .await
    }

    /// `SessionStart` observers; injected context becomes hidden setup messages.
    pub async fn fire_session_start(
        &self,
        source: neenee_core::SessionSource,
        messages: &mut Vec<Message>,
    ) {
        self.hooks()
            .session_start(
                source,
                &self.hook_session_id(),
                self.hook_cwd().as_deref(),
                messages,
            )
            .await
    }

    /// `SessionEnd` observers. Informational only.
    pub async fn fire_session_end(&self) {
        self.hooks()
            .session_end(&self.hook_session_id(), self.hook_cwd().as_deref())
            .await
    }

    /// Between tool rounds, if context pressure exceeds the configured budget,
    /// hand the live message list to the [`ContextProjectionGate`] so it can
    /// produce and persist the next model-visible window.
    async fn project_context_if_needed(
        &self,
        messages: &mut Vec<Message>,
        cancel: &CancellationToken,
    ) -> Result<(), HarnessError> {
        let budget = *self
            .context_prune_threshold_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if budget == 0 || estimate_tokens(messages) <= budget {
            return Ok(());
        }
        let gate = self
            .context_projection_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(gate) = gate else {
            return Ok(());
        };
        let replacement = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
            replacement = gate.project_context(messages.clone()) => replacement,
        };
        if let Some(replacement) = replacement
            && !replacement.is_empty()
        {
            *messages = replacement;
        }
        Ok(())
    }

    pub fn set_thread_id(&self, thread_id: impl Into<String>) {
        *self.thread_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread_id.into());
    }

    pub fn clear_thread_id(&self) {
        *self.thread_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Current task list snapshot. Read by the harness to mirror into the
    /// session and by the TUI to render the sticky panel.
    pub fn todos(&self) -> neenee_core::TodoList {
        self.todos.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the task list. Used by session-restore paths on resume.
    pub fn set_todos(&self, todos: neenee_core::TodoList) {
        *self.todos.lock().unwrap_or_else(|e| e.into_inner()) = todos;
    }

    /// Drop the task list.
    pub fn clear_todos(&self) {
        *self.todos.lock().unwrap_or_else(|e| e.into_inner()) = neenee_core::TodoList::default();
    }

    /// Current harness turn counter — bumped at the start of every
    /// `execute_round`. Used by the TUI to detect a stale task panel (one
    /// whose `updated_at_turn` lags the current turn by more than
    /// `TODO_STALE_TURN_THRESHOLD`).
    pub fn turn_count(&self) -> u64 {
        *self.turn_counter.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Advance the turn counter. Called once per `execute_round`. The TUI
    /// reads the resulting value to compute "not updated for N turns".
    pub fn bump_turn(&self) {
        let mut g = self.turn_counter.lock().unwrap_or_else(|e| e.into_inner());
        *g = g.saturating_add(1);
    }

    pub fn get_unattended(&self) -> bool {
        self.permissions.unattended()
    }

    pub fn set_unattended(&self, enabled: bool) {
        self.permissions.set_unattended(enabled);
    }

    /// Set this agent's operation boundary (ADR-0028). The main agent leaves it
    /// unrestricted; `EnvoyTool` sets the scope resolved from the bound
    /// envoy profile on the child before it runs.
    pub fn set_operation_scope(&self, scope: neenee_core::OperationScope) {
        *self
            .operation_scope
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = scope;
    }

    /// Snapshot of this agent's operation boundary. Used by the `execute_tool`
    /// funnel to gate tools whose target falls outside the granted scope.
    fn operation_scope(&self) -> neenee_core::OperationScope {
        self.operation_scope
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| neenee_core::OperationScope::unrestricted())
    }

    pub fn get_pursuit(&self) -> Option<Pursuit> {
        self.pursuit_state.get()
    }

    pub fn set_pursuit(&self, pursuit: Pursuit) {
        self.pursuit_state.set(pursuit);
    }

    pub fn restore_pursuit(&self, pursuit: Pursuit) {
        self.pursuit_state.restore(pursuit);
    }

    pub fn clear_pursuit(&self) {
        self.pursuit_state.clear();
    }

    pub fn pursuit_can_complete(&self) -> bool {
        self.pursuit_state.can_complete()
    }

    // ── Pursuit stop-gate ───────────────────────────────────────────────
    // `/pursue <condition>` arms the gate. Each time the model would end the
    // turn, the gate re-injects the condition and forces another round until
    // the model signals completion, the safety cap is hit, or the pursuit is
    // disarmed. See [`PursuitState::continuation`].

    pub fn arm_pursuit(&self) {
        self.pursuit_state.arm();
    }

    pub fn disarm_pursuit(&self) {
        self.pursuit_state.disarm();
    }

    pub fn is_pursuit_armed(&self) -> bool {
        self.pursuit_state.is_armed()
    }

    pub fn pursuit_iterations(&self) -> u32 {
        self.pursuit_state.iterations()
    }

    pub(crate) fn pursuit_continuation(&self, response: &Message) -> Option<String> {
        self.pursuit_state
            .continuation(response, MAX_PURSUIT_ITERATIONS)
    }

    /// The turn-end gate (ADR-0025). Combines the `/pursue` stop-gate with any
    /// `Stop` hooks: a pursuit forcing continuation wins; otherwise a `Stop`
    /// hook may force another round with feedback. Returns `None` to let the
    /// turn end — i.e. both the pursuit gate and every Stop hook must agree to
    /// stop. The pursuit gate is queried first so its safety-cap disarm side
    /// effect is preserved.
    ///
    /// Returns the prompt together with the [`InjectionKind`] that produced it,
    /// so the push site stamps the correct provenance (pursuit continuation vs
    /// a `Stop` hook inject) instead of guessing from the text.
    async fn stop_gate(&self, response: &Message) -> Option<(String, InjectionKind)> {
        if let Some(prompt) = self.pursuit_continuation(response) {
            return Some((prompt, InjectionKind::PursuitContinuation));
        }
        self.hooks()
            .check_stop(
                response.content.as_str(),
                &self.hook_session_id(),
                self.hook_cwd().as_deref(),
            )
            .await
            .map(|prompt| (prompt, InjectionKind::Hook(HookEventKind::Stop)))
    }

    pub fn thread_id(&self) -> Option<String> {
        self.thread_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn inject_pursuit_continuation(&self, messages: &mut Vec<Message>) {
        self.pursuit_state.inject_continuation(messages);
    }

    pub fn inject_objective_updated(&self, messages: &mut Vec<Message>) {
        self.pursuit_state.inject_objective_updated(messages);
    }

    pub fn reply_permission(&self, request_id: &str, decision: PermissionDecision) -> bool {
        self.permissions.reply(request_id, decision)
    }

    pub fn reject_pending_permissions(&self) {
        self.permissions.reject_pending();
    }

    pub fn reply_user_question(&self, request_id: &str, answers: Vec<Vec<String>>) -> bool {
        let sender = self
            .ask_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .remove(request_id);
        sender.is_some_and(|sender| {
            sender
                .send(Some(UserQuestionReply {
                    request_id: request_id.to_string(),
                    answers,
                }))
                .is_ok()
        })
    }

    pub fn reject_pending_user_questions(&self) {
        let pending = std::mem::take(
            &mut self
                .ask_user
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending,
        );
        for (_, sender) in pending {
            let _ = sender.send(None);
        }
    }

    /// Resolve a parked interactive-input request (L3.5 β) with the operator's
    /// text, unblocking the `bash` dispatch that issued it. Returns `false` if
    /// no matching request is parked (e.g. already resolved or cancelled).
    /// An empty `text` is a valid "cancel" — the command then runs with
    /// closed stdin and fails fast.
    pub fn reply_input(&self, request_id: &str, text: String) -> bool {
        let sender = self
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .remove(request_id);
        sender.is_some_and(|sender| {
            sender
                .send(Some(InputReply {
                    request_id: request_id.to_string(),
                    text,
                }))
                .is_ok()
        })
    }

    /// Cancel every parked input request (e.g. on turn end / interrupt),
    /// resolving each with `None` so the awaiting dispatch returns a
    /// cancelled result.
    pub fn reject_pending_inputs(&self) {
        let pending =
            std::mem::take(&mut self.input.lock().unwrap_or_else(|e| e.into_inner()).pending);
        for (_, sender) in pending {
            let _ = sender.send(None);
        }
    }

    pub fn allowed_tools(&self) -> Vec<String> {
        self.permissions.allowed_tools()
    }

    pub fn clear_allowed_tools(&self) {
        self.permissions.clear_allowed();
    }

    /// Revoke a single cached "always allow" rule. Returns whether a rule was
    /// actually removed (false if the rule was never cached). Powers the
    /// session modal's per-row revoke.
    pub fn revoke_allowed_tool(&self, tool: &str, scope: &str) -> bool {
        self.permissions.revoke_allowed(tool, scope)
    }

    /// Install (or reuse) the steering inbox and return a [`EnvoyHandle`]
    /// the caller can steer the agent with mid-turn — the entry point of
    /// full-duplex (ADR-0029). Requires `Arc<Self>` because the handle holds a
    /// `Weak<Agent>` so it can observe the agent's lifetime without keeping it
    /// alive after its dispatcher ends the turn.
    ///
    /// Idempotent: the first call creates the `mpsc` pair (sender stored on the
    /// agent so [`Agent::submit`] works too, receiver left for the driver to
    /// `take`); later calls reuse the same pair. The top-level agent driven
    /// directly by the harness never calls this and stays non-steerable by an
    /// inbox — its interrupt path is the `CancellationToken` passed to the run,
    /// and its permission/ask_user replies go through the harness directly.
    pub fn install_inbox(self: &Arc<Self>) -> EnvoyHandle {
        let mut tx_guard = self.inbox_tx.lock().unwrap_or_else(|e| e.into_inner());
        let tx = match tx_guard.clone() {
            Some(existing) => existing,
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                *tx_guard = Some(tx.clone());
                drop(tx_guard);
                *self.inbox_rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
                tx
            }
        };
        EnvoyHandle {
            weak: Arc::downgrade(self),
            ops: tx,
        }
    }

    /// Submit a steering [`AgentOp`] without going through a handle. Equivalent
    /// to [`EnvoyHandle::submit`] but usable when the caller already holds a
    /// reference to the agent rather than a handle (e.g. the top-level harness
    /// steering the primary session). Returns `false` if no inbox was ever
    /// installed ([`Agent::install_inbox`] was not called) or the receiver was
    /// dropped.
    pub fn submit(&self, op: AgentOp) -> bool {
        self.inbox_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|tx| tx.send(op).is_ok())
    }

    /// Drain every op currently buffered in the inbox and apply it to the live
    /// turn. Called by the driver at the top of every tool round (the only
    /// place it is safe to mutate `messages` or end the turn).
    ///
    /// Returns `false` when an `Interrupt` / `Shutdown` was observed — the
    /// caller then returns [`HarnessError::Interrupted`] (`Shutdown` is the
    /// same flow today; a future graceful variant would distinguish them).
    /// `None` for `rx` (no inbox installed) is a no-op that returns `true`, so
    /// non-steerable agents pay nothing.
    fn drain_inbox(
        &self,
        rx: &mut Option<mpsc::UnboundedReceiver<AgentOp>>,
        messages: &mut Vec<Message>,
    ) -> bool {
        let Some(rx) = rx.as_mut() else {
            return true;
        };
        let mut interrupted = false;
        while let Ok(op) = rx.try_recv() {
            match op {
                AgentOp::InjectUserMessage(text) => {
                    messages.push(
                        Message::new(Role::User, text)
                            .with_origin(InjectionOrigin::new(InjectionKind::EnvoySteer)),
                    );
                }
                AgentOp::InterAgentMessage { msg } => {
                    messages.push(Message::injected(
                        Role::User,
                        msg,
                        InjectionOrigin::new(InjectionKind::InterAgent),
                    ));
                }
                AgentOp::Interrupt | AgentOp::Shutdown => {
                    interrupted = true;
                }
            }
        }
        !interrupted
    }

    /// Structured view of the cached "always allow" rules, for the session
    /// modal's Permissions pane. Unlike [`Agent::allowed_tools`] (which collapses
    /// each rule to a single formatted string), this keeps the tool/scope pair
    /// intact so the modal can target an individual rule for revocation.
    pub fn allowed_tools_structured(&self) -> Vec<neenee_core::PermissionRuleInfo> {
        self.permissions.allowed_tools_structured()
    }

    /// Designate the project whose bucket backs the persistent "always"
    /// allowlist, and load any rules already on disk into the in-memory set.
    /// Pass `None` to disable persistence (envoys and most tests do this).
    ///
    /// Loading is best-effort: a missing, unreadable, or unsupported file is
    /// silently ignored — the agent simply starts with an empty allowlist and
    /// re-prompts the user. This is the cross-session hook: a fresh session in
    /// the same project inherits prior `Always` approvals without re-asking.
    pub fn set_project_root(&self, root: Option<std::path::PathBuf>) {
        self.permissions.set_project_root(root);
    }

    /// Seed declarative permission rules from `[permissions]` config. Delegates
    /// to `PermissionStore::seed_from_config`.
    pub fn seed_permissions_from_config(
        &self,
        rules: &[neenee_store::config::PermissionRuleConfig],
    ) {
        self.permissions.seed_from_config(rules);
    }

    /// Replace the entire set of dynamically-refreshable MCP tools. Called by
    /// the `McpCatalog` background refresh loop
    /// after reconnecting servers and re-discovering their tools. The built-in
    /// built-in capabilities are untouched.
    pub fn replace_mcp_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        *self.mcp_tools.write().unwrap_or_else(|e| e.into_inner()) = tools;
    }

    /// A clone of the shared MCP-tools holder, for passing to a
    /// `McpCatalog` so its background refresh can
    /// update the live tool list.
    pub fn mcp_tools_holder(&self) -> Arc<std::sync::RwLock<Vec<Arc<dyn Tool>>>> {
        Arc::clone(&self.mcp_tools)
    }

    /// Set the session-level enabled flag for a tool. No-op when the name is
    /// unknown (so a stale toggle from the modal cannot poison the dispatch
    /// table). Returns whether the flag actually changed.
    pub fn set_tool_enabled(&self, name: &str, enabled: bool) -> bool {
        let known = self.toolset.variants_of(name).is_some();
        if !known {
            return false;
        }
        let mut guard = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let currently_enabled = !guard.contains(name);
        if enabled == currently_enabled {
            return false;
        }
        if enabled {
            guard.remove(name);
        } else {
            guard.insert(name.to_string());
        }
        true
    }

    /// Whether `name` is currently enabled (i.e. visible to the model and
    /// dispatchable). Unknown tools report `false`.
    pub fn is_tool_enabled(&self, name: &str) -> bool {
        let guard = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        !guard.contains(name)
    }

    /// All installed tools that the model may see this turn: every tool whose
    /// name is not in the disabled mask. Used at the schema-build choke points
    /// so a disabled tool's definition never reaches the provider.
    pub(crate) fn visible_tools(&self) -> Vec<Arc<dyn Tool>> {
        let disabled = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let is_visible = |t: &Arc<dyn Tool>| !disabled.contains(t.name());
        let mut tools: Vec<Arc<dyn Tool>> = self
            .resolved_tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|t| is_visible(t))
            .cloned()
            .collect();
        // Merge dynamically-refreshable MCP tools.
        if let Ok(mcp) = self.mcp_tools.read() {
            tools.extend(mcp.iter().filter(|t| is_visible(t)).cloned());
        }
        tools
    }

    /// Structured view of every installed tool, for the session modal's Tools
    /// pane. `enabled` reflects the disabled mask; `source` classifies origin
    /// (`builtin` / `mcp:<server>` / `plan`) from the tool's name.
    pub fn snapshot_tools(&self) -> Vec<neenee_core::ToolInfo> {
        let disabled = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let envoy = ["envoy"];
        // Merge the resolved built-in view and dynamically-refreshable MCP
        // tools into one owned list (one variant per capability).
        let mut all_tools: Vec<Arc<dyn Tool>> = self
            .resolved_tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Ok(guard) = self.mcp_tools.read() {
            all_tools.extend(guard.iter().cloned());
        }
        let mut infos: Vec<neenee_core::ToolInfo> = all_tools
            .iter()
            .map(|t| {
                let name = t.name();
                let source = if let Some(rest) = name.strip_prefix("mcp__") {
                    let server = rest.split("__").next().unwrap_or(rest);
                    format!("mcp:{}", server)
                } else if envoy.contains(&name) {
                    "envoy".to_string()
                } else {
                    "builtin".to_string()
                };
                neenee_core::ToolInfo {
                    name: name.to_string(),
                    description: t.description().to_string(),
                    enabled: !disabled.contains(name),
                    source,
                }
            })
            .collect();
        infos.sort_by(|a, b| a.source.cmp(&b.source).then_with(|| a.name.cmp(&b.name)));
        infos
    }

    /// Structured view of the skills registry, for the session modal's Skills
    /// pane. Mirrors [`skills::RegistryGuard::list`] into the render-friendly
    /// DTO.
    pub fn snapshot_skills(&self) -> Vec<neenee_core::SkillInfo> {
        let guard = self.skills_registry.lock();
        guard
            .list()
            .into_iter()
            .map(|skill| neenee_core::SkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
                version: skill.version.clone(),
                enabled: skill.enabled,
                source: skill.scope.to_string(),
                tags: skill.tags.clone(),
            })
            .collect()
    }

    pub async fn run(&self, messages: &mut Vec<Message>) -> Result<RoundOutcome, HarnessError> {
        // Non-interactive convenience path: not cancellable from the outside.
        self.run_with_events(messages, &CancellationToken::new(), |event| match event {
            AgentEvent::PermissionRequest(request) => {
                self.reply_permission(&request.id, PermissionDecision::Reject);
            }
            AgentEvent::UserQuestionRequest(request) => {
                self.reply_user_question(&request.id, Vec::new());
            }
            _ => {}
        })
        .await
    }

    #[tracing::instrument(skip_all, name = "turn", fields(streaming = false))]
    pub async fn run_with_events<F>(
        &self,
        messages: &mut Vec<Message>,
        cancel: &CancellationToken,
        mut on_event: F,
    ) -> Result<RoundOutcome, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        // Only enabled tools are advertised to the model: the disabled mask is
        // applied here so a toggled-off tool's schema never reaches the
        // provider, which keeps the model from naming it in the first place.
        let visible = self.visible_tools();
        self.provider.prepare_tools(&visible);
        let turn_start = std::time::Instant::now();
        let mut state = TurnState {
            guards: TurnState::guards_default(self.nudge_config()),
            ..TurnState::default()
        };
        let mut tool_rounds = 0;
        // Take the steering inbox receiver for this turn (ADR-0029). `None` for
        // a non-steerable agent (no `install_inbox` call) → `drain_inbox` is a
        // no-op. Taken once per agent: a re-run after the first returns `None`
        // too, which is fine for the top-level harness (driven directly) and
        // for envoys (single run).
        let mut inbox_rx = self
            .inbox_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        loop {
            if cancel.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            // Apply any steering ops queued since the last round (inject a
            // message, or abort via Interrupt/Shutdown) before requesting the
            // next completion. Replies (permission/ask_user) do NOT flow here
            // — they resolve the parked oneshot directly.
            if !self.drain_inbox(&mut inbox_rx, messages) {
                return Err(HarnessError::Interrupted);
            }

            self.prepare_turn_messages(messages);
            on_event(AgentEvent::ModelRequestStarted {
                tool_round: tool_rounds,
            });

            let response = match tokio::time::timeout(
                CHAT_RESPONSE_TIMEOUT,
                self.provider.chat(messages.clone()),
            )
            .await
            {
                Ok(result) => result?,
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = CHAT_RESPONSE_TIMEOUT.as_secs(),
                        "non-streaming chat request timed out"
                    );
                    return Err(HarnessError::Retryable {
                        message: format!(
                            "Provider did not respond within {} seconds.",
                            CHAT_RESPONSE_TIMEOUT.as_secs()
                        ),
                        retry_after_ms: None,
                    });
                }
            };
            if !valid_assistant_response(&response) {
                return Err(empty_response_error(&response));
            }
            self.book_turn_usage(&mut state, &response, None);
            messages.push(response.clone());

            // The model produced no text stream, so nothing was shown to the UI
            // that a fallback tool call would need to retract.
            if self
                .dispatch_tool_calls(
                    &response,
                    messages,
                    &mut state,
                    false,
                    cancel,
                    &mut on_event,
                )
                .await?
            {
                tool_rounds += 1;
                if self.check_hard_stop(tool_rounds).is_break() {
                    return Err(self.hard_stop_error());
                }
                self.project_context_if_needed(messages, cancel).await?;
                // Mid-turn save point (ADR-0035): see the streaming path.
                self.fire_round_persist(messages).await?;
                self.apply_guard_actions(messages, &mut state, &mut on_event);
                self.run_turn_hooks(messages, &state, tool_rounds).await;
                continue;
            }

            // Pursuit stop-gate: if a pursuit is armed and the model has not
            // signalled completion, re-inject the condition and force another
            // round instead of ending the turn.
            if let Some((prompt, kind)) = self.stop_gate(&response).await {
                self.pursuit_state.bump_iterations();
                messages.push(Message::injected(
                    Role::User,
                    prompt,
                    InjectionOrigin::new(kind),
                ));
                continue;
            }

            return Ok(RoundOutcome {
                message: response,
                token_usage: state.token_usage,
                duration_ms: turn_start.elapsed().as_millis() as u64,
            });
        }
    }

    #[tracing::instrument(skip_all, name = "turn", fields(streaming = true))]
    pub async fn run_streaming_with_events<F>(
        &self,
        messages: &mut Vec<Message>,
        cancel: &CancellationToken,
        mut on_event: F,
    ) -> Result<RoundOutcome, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        // Only enabled tools are advertised to the model: the disabled mask is
        // applied here so a toggled-off tool's schema never reaches the
        // provider, which keeps the model from naming it in the first place.
        let visible = self.visible_tools();
        self.provider.prepare_tools(&visible);
        let turn_start = std::time::Instant::now();
        let mut state = TurnState {
            guards: TurnState::guards_default(self.nudge_config()),
            ..TurnState::default()
        };
        let mut tool_rounds = 0;
        // Take the steering inbox receiver for this turn (ADR-0029). See
        // `run_with_events` for rationale; same dual-no-op contract for a
        // non-steerable agent.
        let mut inbox_rx = self
            .inbox_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        loop {
            if cancel.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            // Apply steering ops queued since the last round before requesting
            // the next stream. Replies bypass this (see drain_inbox).
            if !self.drain_inbox(&mut inbox_rx, messages) {
                return Err(HarnessError::Interrupted);
            }

            self.prepare_turn_messages(messages);
            tracing::debug!(tool_round = tool_rounds, "requesting model completion");
            on_event(AgentEvent::ModelRequestStarted {
                tool_round: tool_rounds,
            });
            // Race the model request against cancellation so an interrupt
            // while we're waiting on the network resolves promptly instead of
            // blocking until the first stream chunk arrives. The idle-timeout
            // arm covers a provider endpoint that accepts the connection but
            // never sends HTTP response headers (overloaded upstream, dropped
            // proxy) — without it the select would hang forever on `.send()`.
            let mut stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
                result = tokio::time::timeout(
                    STREAM_IDLE_TIMEOUT,
                    self.provider.stream_chat_events(messages.clone()),
                ) => match result {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => return Err(HarnessError::from(error)),
                    Err(_elapsed) => {
                        tracing::warn!(
                            timeout_secs = STREAM_IDLE_TIMEOUT.as_secs(),
                            "stream request timed out before any response"
                        );
                        return Err(HarnessError::Retryable {
                            message: format!(
                                "Provider did not start streaming within {} seconds.",
                                STREAM_IDLE_TIMEOUT.as_secs()
                            ),
                            retry_after_ms: None,
                        });
                    }
                },
            };
            let mut content = String::new();
            let mut reasoning_content = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut emitted_text = false;
            let mut emitted_reasoning = false;
            // Token usage reported mid-stream via a `Usage` event (OpenAI
            // `include_usage`, Anthropic `message_delta`). Captured here and
            // preferred over the local estimate when booking the turn.
            let mut streamed_usage: Option<TokenUsage> = None;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
                    // Guard against a stalled SSE stream: providers use
                    // `reqwest::Client::new()` with no read timeout, so without
                    // this bound a connection that stays open but stops sending
                    // (common with overloaded reasoning-model endpoints) blocks
                    // the turn forever. The idle clock resets on every chunk,
                    // so a legitimately slow reasoning model that keeps
                    // trickling deltas is never cut off.
                    event = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()) => {
                        let event = match event {
                            Ok(Some(event)) => event,
                            Ok(None) => break,
                            Err(_elapsed) => {
                                tracing::warn!(
                                    idle_timeout_secs = STREAM_IDLE_TIMEOUT.as_secs(),
                                    "stream stalled: no data received within idle timeout"
                                );
                                return Err(HarnessError::Retryable {
                                    message: format!(
                                        "Provider stream stalled — no data received \
                                         for {} seconds.",
                                        STREAM_IDLE_TIMEOUT.as_secs()
                                    ),
                                    retry_after_ms: None,
                                });
                            }
                        };
                        match event? {
                            ProviderStreamEvent::TextDelta(delta) => {
                                content.push_str(&delta);
                                on_event(AgentEvent::AssistantDelta {
                                    delta,
                                    start: !emitted_text,
                                });
                                emitted_text = true;
                            }
                            ProviderStreamEvent::ReasoningDelta(delta) => {
                                reasoning_content.push_str(&delta);
                                on_event(AgentEvent::ReasoningDelta {
                                    delta,
                                    start: !emitted_reasoning,
                                });
                                emitted_reasoning = true;
                            }
                            ProviderStreamEvent::ToolCallDelta {
                                index,
                                id,
                                name,
                                arguments,
                            } => {
                                while calls.len() <= index {
                                    calls.push(ToolCall {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: String::new(),
                                    });
                                }
                                let call = &mut calls[index];
                                if let Some(id) = id {
                                    call.id.push_str(&id);
                                }
                                if let Some(name) = name {
                                    call.name.push_str(&name);
                                }
                                call.arguments.push_str(&arguments);
                            }
                            ProviderStreamEvent::Usage(usage) => {
                                // Take the last reported usage (providers may
                                // emit one final usage chunk). Prefer it over
                                // the local estimate at booking time.
                                streamed_usage = Some(usage);
                            }
                        }
                    }
                }
            }
            if emitted_text {
                on_event(AgentEvent::AssistantEnd(content.clone()));
            }
            if emitted_reasoning {
                on_event(AgentEvent::ReasoningEnd(reasoning_content.clone()));
            }

            calls.retain(|call| !call.name.is_empty());
            for call in &mut calls {
                if call.id.is_empty() {
                    call.id = format!("call_{}", uuid::Uuid::new_v4());
                }
            }
            let response = Message {
                role: Role::Assistant,
                content,
                content_blob: None,
                display_content: None,
                reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
                tool_calls: (!calls.is_empty()).then_some(calls),
                tool_call_id: None,
                images: None,
                // Stamp which provider/model produced this turn so a session
                // that mixes models stays traceable after resume. The proxy
                // provider delegates to whichever concrete provider is active.
                provider: Some(self.provider.provider_id()),
                model: Some(self.provider.model()),
                // Drain any provider-opaque wire credential the turn
                // accumulated (e.g. the Anthropic thinking signature) so the
                // next replay re-emits it verbatim. None for providers that
                // carry none; the map is opaque to this layer.
                provider_meta: self.provider.take_last_provider_meta(),
                hidden: false,
                children: None,
                envoy_meta: None,
                origin: None,
            };
            if !valid_assistant_response(&response) {
                return Err(empty_response_error(&response));
            }
            self.book_turn_usage(&mut state, &response, streamed_usage.take());
            messages.push(response.clone());

            // `emitted_text` means assistant text was already streamed to the
            // UI; a text-fallback tool call must then retract it via a discard.
            if self
                .dispatch_tool_calls(
                    &response,
                    messages,
                    &mut state,
                    emitted_text,
                    cancel,
                    &mut on_event,
                )
                .await?
            {
                tool_rounds += 1;
                if self.check_hard_stop(tool_rounds).is_break() {
                    return Err(self.hard_stop_error());
                }
                self.project_context_if_needed(messages, cancel).await?;
                // Mid-turn save point (ADR-0035): persist this round's new
                // messages (the assistant response + all tool results) before
                // any further work, so a crash leaves the transcript in sync
                // with filesystem side effects.
                self.fire_round_persist(messages).await?;
                self.apply_guard_actions(messages, &mut state, &mut on_event);
                self.run_turn_hooks(messages, &state, tool_rounds).await;
                continue;
            }

            // Pursuit stop-gate (mirror of the non-streaming path): if a
            // pursuit is armed and the model has not signalled completion,
            // re-inject the condition and force another round.
            if let Some((prompt, kind)) = self.stop_gate(&response).await {
                self.pursuit_state.bump_iterations();
                messages.push(Message::injected(
                    Role::User,
                    prompt,
                    InjectionOrigin::new(kind),
                ));
                continue;
            }

            return Ok(RoundOutcome {
                message: response,
                token_usage: state.token_usage,
                duration_ms: turn_start.elapsed().as_millis() as u64,
            });
        }
    }

    /// Execute any tool calls carried by `response`, emitting events and
    /// appending tool results to `messages`. Shared by the streaming and
    /// non-streaming loops so the dispatch contract — repeated-call guard,
    /// up-front `ToolCall` events, concurrent execution with FIFO-ordered
    /// results, and pursuit/mode updates — lives in exactly one place.
    ///
    /// `streamed_text` is true when the response text was already streamed to
    /// the UI, so a recognised text-fallback tool call retracts it with an
    /// `AssistantDiscard`. Returns `true` when a tool round ran (the caller
    /// should loop again), `false` when the turn is complete.
    ///
    /// `cancel` makes tool execution cooperative: if the turn is interrupted
    /// mid-flight, every already-announced [`AgentEvent::ToolCall`] is paired
    /// with a terminal [`AgentEvent::ToolCancelled`] before this returns
    /// `Err(HarnessError::Interrupted)`, so no step is left "running".
    async fn dispatch_tool_calls<F>(
        &self,
        response: &Message,
        messages: &mut Vec<Message>,
        state: &mut TurnState,
        streamed_text: bool,
        cancel: &CancellationToken,
        on_event: &mut F,
    ) -> Result<bool, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        // Native tool calls (OpenAI-style function calling). An empty list is
        // treated as "no tool calls" so we fall through to the text fallback.
        if let Some(tool_calls) = response
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            // Classify this round once, for two consumers: the round-hook axis
            // (consecutive read-only streak, surfaced to user hooks) and the
            // turn-guard registry (checked at the round boundary). Any call
            // whose target is a real Path/Command (i.e. not Unspecified) makes
            // the round "progress", resetting both.
            let all_read = tool_calls
                .iter()
                .all(|c| self.tool_target_is_unspecified(&c.name, &c.arguments));
            if all_read {
                state.consecutive_readonly_rounds =
                    state.consecutive_readonly_rounds.saturating_add(1);
            } else {
                state.consecutive_readonly_rounds = 0;
            }
            // Feed the round's tool-call data to the guard state. The guards
            // consume it at the round boundary.
            let round_calls: Vec<(String, String)> = tool_calls
                .iter()
                .map(|c| (c.name.clone(), c.arguments.clone()))
                .collect();
            state.guards.set_round(round_calls, all_read);
            // Emit all ToolCall events up front.
            let call_ids: Vec<String> = tool_calls
                .iter()
                .map(|_| format!("call_{}", uuid::Uuid::new_v4()))
                .collect();
            tracing::info!(count = tool_calls.len(), "dispatching native tool calls");
            for (call, id) in tool_calls.iter().zip(&call_ids) {
                tracing::debug!(tool = %call.name, "tool call");
                on_event(AgentEvent::ToolCall {
                    id: id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
            }
            // Signature-level loop guard (ADR-0036): a read call whose canonical
            // signature is in the per-turn block mask — set by a guard that saw
            // the model repeat it past a nudge — is short-circuited here, before
            // execution. The model gets an explanatory error instead of the
            // content, so it is physically unable to re-enter the loop. Only
            // read-tier calls can be masked; writes/commands are never blocked
            // by the read-loop guard. Blocked calls are split out so the rest
            // run concurrently exactly as before. Each blocked call is given the
            // same ToolResult event an executed call would get, so the UI never
            // sees an orphaned running step.
            let blocked_output = |name: &str| {
                ToolOutput::Text(format!(
                    "[loop guard] This read ({name}) is blocked for the rest of the turn \
                     because it was repeated past a steering warning. Re-reading it \
                     cannot help: the content is already in context above. Act on it \
                     now (e.g. use edit_file/write_file with the text you already \
                     captured), read a *different* file or line range, or, if you \
                     cannot proceed, say so explicitly or call `abort`."
                ))
            };
            let mut results: Vec<Option<(ToolOutput, u64)>> =
                (0..tool_calls.len()).map(|_| None).collect();
            let exec_indices: Vec<usize> = tool_calls
                .iter()
                .enumerate()
                .filter(|(idx, c)| {
                    if self.tool_target_is_unspecified(&c.name, &c.arguments)
                        && state.guards.is_blocked(&c.name, &c.arguments)
                    {
                        tracing::warn!(
                            tool = %c.name,
                            args = %c.arguments,
                            "read blocked by turn-loop guard signature mask"
                        );
                        let output = blocked_output(&c.name);
                        let id = &call_ids[*idx];
                        // Emit the ToolResult the executed path would have, so
                        // the UI pairs this call's ToolCall with a terminal
                        // result instead of leaving it "running".
                        on_event(AgentEvent::Notice(
                            AgentNotice::new(
                                NoticeKind::NudgeInjected,
                                NoticeSeverity::Warning,
                                "Blocked repeating read",
                                NoticeSource::TurnGuard,
                            )
                            .with_body(format!(
                                "A read ({}) was blocked by the loop guard — it is the same \
                                 read the agent has repeated past a warning. Use the content \
                                 already in context, or read something different.",
                                c.name,
                            ))
                            .with_surface(NoticeSurface::Toast),
                        ));
                        on_event(AgentEvent::ToolResult {
                            id: id.clone(),
                            name: c.name.clone(),
                            output: output.to_text(),
                            structured: output.clone(),
                            duration_ms: 0,
                        });
                        results[*idx] = Some((output, 0));
                        false // do not execute
                    } else {
                        true // execute
                    }
                })
                .map(|(idx, _)| idx)
                .collect();
            if !exec_indices.is_empty() {
                let exec_calls: Vec<ToolCall> = exec_indices
                    .iter()
                    .map(|&i| tool_calls[i].clone())
                    .collect();
                let exec_ids: Vec<String> =
                    exec_indices.iter().map(|&i| call_ids[i].clone()).collect();
                let exec_results = self
                    .execute_tools_concurrent(&exec_calls, &exec_ids, cancel, on_event)
                    .await?;
                for (pos, &idx) in exec_indices.iter().enumerate() {
                    results[idx] = exec_results.get(pos).cloned();
                }
            }
            // Flatten back to a positional Vec, matching tool_calls order.
            let results: Vec<(ToolOutput, u64)> = results
                .into_iter()
                .map(|r| {
                    r.unwrap_or_else(|| (ToolOutput::Text("[loop guard] blocked".to_string()), 0))
                })
                .collect();
            let denied = results
                .iter()
                .any(|(result, _)| matches!(result, ToolOutput::PermissionDenied { .. }));
            for ((call, id), (result, duration_ms)) in tool_calls.iter().zip(&call_ids).zip(results)
            {
                self.record_tool_result(
                    call,
                    id,
                    &result,
                    duration_ms,
                    messages,
                    state,
                    false,
                    on_event,
                );
                self.run_post_tool_hooks(call, &result, duration_ms, messages)
                    .await;
            }
            // If the user denied permission for any call, stop the turn here
            // instead of feeding the (possibly partial) results back to the
            // model and asking it to continue.
            return Ok(!denied);
        }

        // Text-based fallback: any provider may emit a JSON tool call as text.
        if let Some(call) = tool_call::parse_text_tool_call(&response.content) {
            if streamed_text {
                on_event(AgentEvent::AssistantDiscard);
            }
            tracing::debug!(tool = %call.name, "tool call (text fallback)");
            tool_call::attach_fallback_tool_call(messages, &call);
            // Classify + feed this round to the guard, mirroring the native
            // path. The text-fallback emits one call per round, so a read-only
            // round is exactly "this single call is read-tier". Without this the
            // guard would never see text-fallback rounds and a model on such a
            // provider could loop with zero coverage.
            let all_read = self.tool_target_is_unspecified(&call.name, &call.arguments);
            if all_read {
                state.consecutive_readonly_rounds =
                    state.consecutive_readonly_rounds.saturating_add(1);
            } else {
                state.consecutive_readonly_rounds = 0;
            }
            state
                .guards
                .set_round(vec![(call.name.clone(), call.arguments.clone())], all_read);
            let call_id = format!("call_{}", uuid::Uuid::new_v4());
            on_event(AgentEvent::ToolCall {
                id: call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            // Signature-level loop guard: short-circuit a blocked read before it
            // executes (ADR-0036). Same contract as the native path — the model
            // gets an explanatory error instead of the content.
            let result = if all_read && state.guards.is_blocked(&call.name, &call.arguments) {
                tracing::warn!(
                    tool = %call.name,
                    args = %call.arguments,
                    "text-fallback read blocked by turn-loop guard signature mask"
                );
                on_event(AgentEvent::Notice(
                    AgentNotice::new(
                        NoticeKind::NudgeInjected,
                        NoticeSeverity::Warning,
                        "Blocked repeating read",
                        NoticeSource::TurnGuard,
                    )
                    .with_body(format!(
                        "A read ({}) was blocked by the loop guard — it is the same \
                         read the agent has repeated past a warning. Use the content \
                         already in context, or read something different.",
                        call.name,
                    ))
                    .with_surface(NoticeSurface::Toast),
                ));
                let output = ToolOutput::Text(format!(
                    "[loop guard] This read ({}) is blocked for the rest of the turn \
                     because it was repeated past a steering warning. Re-reading it \
                     cannot help: the content is already in context above. Act on it \
                     now (e.g. use edit_file/write_file with the text you already \
                     captured), read a *different* file or line range, or, if you \
                     cannot proceed, say so explicitly or call `abort`.",
                    call.name,
                ));
                on_event(AgentEvent::ToolResult {
                    id: call_id.clone(),
                    name: call.name.clone(),
                    output: output.to_text(),
                    structured: output.clone(),
                    duration_ms: 0,
                });
                output
            } else {
                self.execute_tool_evented(&call, &call_id, cancel, on_event)
                    .await?
            };
            let denied = matches!(result, ToolOutput::PermissionDenied { .. });
            let duration_ms = std::time::Instant::now().elapsed().as_millis() as u64;
            self.record_tool_result(
                &call,
                &call_id,
                &result,
                duration_ms,
                messages,
                state,
                true,
                on_event,
            );
            self.run_post_tool_hooks(&call, &result, duration_ms, messages)
                .await;
            return Ok(!denied);
        }

        Ok(false)
    }

    /// Account for, surface, and persist a single tool result. The argument
    /// count reflects the per-result state it must thread; grouping it further
    /// would only move the noise to the call sites.
    #[allow(clippy::too_many_arguments)]
    fn record_tool_result<F>(
        &self,
        call: &ToolCall,
        call_id: &str,
        result: &ToolOutput,
        duration_ms: u64,
        messages: &mut Vec<Message>,
        state: &mut TurnState,
        emit_event: bool,
        on_event: &mut F,
    ) where
        F: FnMut(AgentEvent) + Send,
    {
        let text = result.to_text();
        // Cost attribution: an envoy's true token consumption can be 100x
        // the byte-estimate of its final summary, so accumulate the real
        // `TokenUsage` it reported. For every other tool the byte-estimate
        // remains the only signal we have.
        match result.envoy_payload() {
            Some((_sub_messages, sub_usage)) => {
                state.token_usage.total_tokens += sub_usage.total_tokens;
                state.token_usage.prompt_tokens += sub_usage.prompt_tokens;
                state.token_usage.completion_tokens += sub_usage.completion_tokens;
                // Still count the summary bytes that the parent model will
                // actually re-read on the next round.
                state.token_usage.total_tokens += pressure::estimate_string_tokens(&text);
            }
            None => {
                state.token_usage.total_tokens += pressure::estimate_string_tokens(&text);
            }
        }
        tracing::info!(tool = %call.name, duration_ms, bytes = text.len(), "tool result");
        self.emit_todos_change(call, on_event);
        if emit_event {
            on_event(AgentEvent::ToolResult {
                id: call_id.to_string(),
                name: call.name.clone(),
                output: text.clone(),
                structured: result.clone(),
                duration_ms,
            });
        }
        // For envoy results, attach the nested transcript as `children` on
        // the persisted Tool-role message so resume can rebuild the envoy
        // view without a live event stream. The nested `Message`s already
        // self-contain their own tool_calls / tool_call_id / children, so
        // arbitrarily deep envoy trees round-trip through session.json.
        // Sidecar `envoy_meta` captures what the live event stream knew but
        // the bare transcript cannot reconstruct on resume: duration, the
        // task description, the toolset size, and an explicit failure flag.
        let tool_message = match result.envoy_payload() {
            Some((sub_messages, _)) => {
                let meta = crate::message::EnvoyMeta {
                    duration_ms: Some(duration_ms),
                    failed: result.is_error(),
                    ..Default::default()
                };
                Message::tool_result(call, format!("[{} result]:\n{}", call.name, text))
                    .with_children(sub_messages.to_vec())
                    .with_envoy_meta(meta)
            }
            None => Message::tool_result(call, format!("[{} result]:\n{}", call.name, text)),
        };
        messages.push(tool_message);

        // Image peel-out (mirrors opencode's OpenAI-Chat lowering). The tool
        // message only carries text (OpenAI Chat Completions requires tool
        // content to be a string), so the actual image is injected as a
        // follow-up user-role message with the image attached — the same
        // channel paste-up uses. The provider serialises it to `image_url`
        // (OpenAI-compat) / `inline_data` (Gemini), letting the model see the
        // pixels. A short textual link ties the two messages together.
        if let ToolOutput::Image { mime, data } = result {
            let image_msg = Message::new(Role::User, format!("Image from {}", call.name))
                .with_images(vec![ImagePart {
                    mime: mime.clone(),
                    data: data.clone(),
                }]);
            messages.push(image_msg);
        }
    }

    /// Fire PostToolUse (success) or PostToolUseFailure (error) hooks and append
    /// any injected context as hidden user messages (ADR-0025). No-op when the
    /// registry is empty, which is the common case (envoys, tests, no
    /// `[hooks]` config).
    async fn run_post_tool_hooks(
        &self,
        call: &ToolCall,
        result: &ToolOutput,
        duration_ms: u64,
        messages: &mut Vec<Message>,
    ) {
        let registry = self.hooks();
        if registry.is_empty() {
            return;
        }
        let summary = result.to_text();
        let session_id = self.hook_session_id();
        let cwd = self.hook_cwd();
        let is_error = result.is_error();
        let injected = if is_error {
            registry
                .run_post_tool_use_failure(
                    call.name.as_str(),
                    &summary,
                    &session_id,
                    cwd.as_deref(),
                )
                .await
        } else {
            registry
                .run_post_tool_use(
                    call.name.as_str(),
                    &summary,
                    duration_ms,
                    &session_id,
                    cwd.as_deref(),
                )
                .await
        };
        let kind = if is_error {
            InjectionKind::Hook(HookEventKind::PostToolUseFailure)
        } else {
            InjectionKind::Hook(HookEventKind::PostToolUse)
        };
        for context in injected {
            messages.push(Message::injected(
                Role::User,
                context,
                InjectionOrigin::new(kind),
            ));
        }
    }

    /// Whether a tool call's [`ScopeTarget`] is [`ScopeTarget::Unspecified`] —
    /// i.e. the tool declares no locatable target (a pure read/search like
    /// `read_text`, `grep`). Used to classify a round as read-only for the
    /// round-hook streak counter. An unknown tool name reads as `true`
    /// (unspecified), matching the trait default.
    fn tool_target_is_unspecified(&self, name: &str, arguments: &str) -> bool {
        match self
            .resolved_tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| t.name() == name)
        {
            Some(t) => matches!(
                t.scope_target(arguments),
                neenee_core::ScopeTarget::Unspecified
            ),
            None => true,
        }
    }

    /// Fire user-configured `Turn` hooks at the turn boundary and fold any
    /// `Inject` context into hidden user messages. `Deny` is already discarded
    /// by [`HookRegistry::run_turn`], so a turn hook cannot abort the round.
    async fn run_turn_hooks(&self, messages: &mut Vec<Message>, state: &TurnState, turn: usize) {
        let registry = self.hooks();
        if registry.is_empty() {
            return;
        }
        let injected = registry
            .run_turn(
                turn,
                state.consecutive_readonly_rounds,
                &self.hook_session_id(),
                self.hook_cwd().as_deref(),
            )
            .await;
        for context in injected {
            messages.push(Message::injected(
                Role::User,
                context,
                InjectionOrigin::new(InjectionKind::Hook(HookEventKind::Turn)),
            ));
        }
    }

    /// Consult the turn-guard registry for the round just dispatched and apply
    /// the resulting action. `Inject` appends a steering nudge as a hidden user
    /// message (non-terminating); `Abort` would terminate the turn. Gated by
    /// the live [`NudgeConfig::enabled`] flag so envoys and the review
    /// diagnostic run unobstructed. Mirrored at both loop boundaries.
    fn apply_guard_actions<F>(
        &self,
        messages: &mut Vec<Message>,
        state: &mut TurnState,
        on_event: &mut F,
    ) where
        F: FnMut(AgentEvent) + Send,
    {
        if !self.nudge_enabled() {
            return;
        }
        match state.guards.take_action() {
            crate::loop_guard::GuardAction::Continue => {}
            crate::loop_guard::GuardAction::Inject(nudge) => {
                tracing::debug!("turn guard tripped; injecting steering nudge");
                on_event(AgentEvent::Notice(
                    AgentNotice::new(
                        NoticeKind::NudgeInjected,
                        NoticeSeverity::Warning,
                        "Steering nudge injected",
                        NoticeSource::TurnGuard,
                    )
                    .with_body(
                        "The agent repeated a read pattern, so a hidden steering note was added before the next model request.",
                    )
                    .with_surface(NoticeSurface::Toast),
                ));
                messages.push(Message::injected(
                    Role::User,
                    nudge,
                    InjectionOrigin::new(InjectionKind::LoopReviewNudge),
                ));
            }
            crate::loop_guard::GuardAction::Block { message, .. } => {
                // `take_action` already recorded the blocked signatures in the
                // per-turn mask, so dispatch will short-circuit any matching
                // read from now on. Here we just surface the rationale to the
                // model (same hidden-message vehicle as a nudge) and notify the
                // UI that the looped read is now hard-blocked.
                tracing::warn!(
                    blocked = ?state.guards.blocked_summary(),
                    "turn guard hard-blocked a looping read signature for the turn"
                );
                on_event(AgentEvent::Notice(
                    AgentNotice::new(
                        NoticeKind::NudgeInjected,
                        NoticeSeverity::Warning,
                        "Looping read blocked",
                        NoticeSource::TurnGuard,
                    )
                    .with_body(
                        "The agent ignored a steering nudge and kept repeating a read, so \
                         that read is now hard-blocked for the rest of the turn. It must \
                         act on what it has, read something different, or call `abort`.",
                    )
                    .with_surface(NoticeSurface::Toast),
                ));
                messages.push(Message::injected(
                    Role::User,
                    message,
                    InjectionOrigin::new(InjectionKind::LoopReviewNudge),
                ));
            }
            crate::loop_guard::GuardAction::Abort(reason) => {
                tracing::warn!("turn guard aborted turn: {reason}");
                on_event(AgentEvent::Notice(
                    AgentNotice::new(
                        NoticeKind::NudgeInjected,
                        NoticeSeverity::Error,
                        "Turn guard aborted the turn",
                        NoticeSource::TurnGuard,
                    )
                    .with_body(reason)
                    .with_surface(NoticeSurface::Banner),
                ));
                // Abort is surfaced as a terminal nudge for now; a future
                // guard that needs a hard turn-kill can return Err here.
            }
        }
    }

    /// The opt-in hard-stop gate (ADR-0018). Called once per tool round with
    /// the count of rounds that have already run this turn. Returns
    /// `ControlFlow::Break` only when a finite `hard_stop_turns` budget was
    /// configured and `rounds` has reached it — the caller converts that into
    /// a terminal `HarnessError` via [`Self::hard_stop_error`]. The default
    /// budget (`0`) keeps the turn uncapped, exactly matching ADR-0009.
    ///
    /// Session review no longer fires from the turn loop: it is on-demand via
    /// `/review` ([`Self::review_now`]), which runs the diagnostic envoy
    /// against the live transcript and reports a verdict without aborting.
    fn check_hard_stop(&self, rounds: usize) -> std::ops::ControlFlow<()> {
        let budget = self.get_hard_stop_turns();
        if budget > 0 && rounds >= budget {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    }

    /// Terminal error surfaced when an opt-in `hard_stop_turns` budget is
    /// exhausted. Echoes the configured budget so the user can tell this apart
    /// from a normal completion in the transcript. The review itself never
    /// produces this — only an explicit user-configured budget does.
    fn hard_stop_error(&self) -> HarnessError {
        let budget = self.get_hard_stop_turns();
        HarnessError::Other(format!(
            "Agent stopped: the configured hard-stop budget of {budget} tool \
             rounds was reached. This budget is opt-in (`hard_stop_turns`); \
             raise it or set it to 0 (the default) for an uncapped turn."
        ))
    }

    /// Collapse a set of review verdicts into one human-facing alert string.
    /// Empty when every dimension is healthy (the TUI treats empty as "clear
    /// any prior alert"). Otherwise the worst status wins, with each
    /// non-healthy dimension's detail folded in. The round count gives the
    /// user a sense of how long the turn has run. Associated (no `&self`) so
    /// the `/review` handler and tests can call it without an `Agent` handle.
    pub fn render_review_alert(verdicts: &[ReviewVerdict], rounds: usize) -> String {
        let worst = verdicts.iter().map(|v| v.status).max();
        match worst {
            None | Some(ReviewStatus::Healthy) => String::new(),
            Some(status) => {
                let label = status.label();
                let details: Vec<&str> = verdicts
                    .iter()
                    .filter(|v| v.status != ReviewStatus::Healthy && !v.detail.trim().is_empty())
                    .map(|v| v.detail.trim())
                    .collect();
                if details.is_empty() {
                    format!("review: {label} · {rounds} rounds — Esc to interrupt")
                } else {
                    format!(
                        "review: {label} · {rounds} rounds — {} — Esc to interrupt",
                        details.join("; ")
                    )
                }
            }
        }
    }

    /// On-demand session review (ADR-0018): run the bounded read-only
    /// diagnostic envoy against `messages` and return one verdict per
    /// registered dimension. Driven by the `/review` command — the harness no
    /// longer fires review on a round cadence. Safe to call while a turn is
    /// running: the reviewer is an independent child agent that only reads a
    /// transcript snapshot and cannot mutate the parent's turn state.
    pub async fn review_now(&self, messages: &[Message]) -> Vec<ReviewVerdict> {
        let rounds = Self::estimate_tool_rounds(messages);
        self.run_session_review(messages, rounds).await
    }

    /// Rough count of tool rounds represented by `messages`: the number of
    /// assistant messages that carry tool calls. Used to label on-demand
    /// review output with a sense of how long the turn has run, since the
    /// `/review` handler does not own the live round counter.
    pub fn estimate_tool_rounds(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter(|m| {
                m.role == Role::Assistant && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
            })
            .count()
    }

    /// Emit a [`AgentEvent::TodosUpdated`] snapshot whenever a tool mutates
    /// the task list (`todo` full-replace or `todo_update` surgical edit).
    /// The TUI stores the snapshot and re-renders the sticky panel above the
    /// input box.
    fn emit_todos_change<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if matches!(call.name.as_str(), "todo" | "todo_update") {
            on_event(AgentEvent::TodosUpdated(self.todos()));
        }
    }

    async fn execute_ask_user(
        &self,
        call: &ToolCall,
        _call_id: &str,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutput::Text(format!("Invalid ask_user arguments: {}", e));
            }
        };
        let questions: Vec<UserQuestion> = match serde_json::from_value(
            args.get("questions")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(q) => q,
            Err(e) => {
                return ToolOutput::Text(format!("Invalid ask_user questions: {}", e));
            }
        };
        if questions.is_empty() {
            return ToolOutput::Text("ask_user requires at least one question.".to_string());
        }
        for (i, q) in questions.iter().enumerate() {
            if q.options.is_empty() {
                return ToolOutput::Text(format!("ask_user question {} has no options.", i + 1));
            }
        }

        let request = UserQuestionRequest {
            id: format!("ask_user_{}", uuid::Uuid::new_v4()),
            questions,
        };
        let (sender, receiver) = oneshot::channel();
        self.ask_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .insert(request.id.clone(), sender);
        tracing::info!(questions = request.questions.len(), "asking user");
        let _ = event_tx.send(AgentEvent::UserQuestionRequest(request.clone()));

        match receiver.await.unwrap_or(None) {
            Some(reply) => {
                let output = serde_json::to_string_pretty(&reply.answers)
                    .unwrap_or_else(|_| format!("{:?}", reply.answers));
                ToolOutput::Text(format!(
                    "User answered the question(s). Selected option labels:\n{}",
                    output
                ))
            }
            None => {
                ToolOutput::Text("User cancelled the question; no answer was provided.".to_string())
            }
        }
    }

    /// Park an interactive-input request for a `bash` command (L3.5 β) and
    /// await the operator's reply. Called from `execute_tool` when the
    /// interactive classifier matches and no model-supplied stdin is
    /// authorized. Emits [`AgentEvent::InputRequest`]; the TUI shows an inline
    /// input panel and the reply travels back via [`Self::reply_input`].
    ///
    /// Returns `Some(StdinPolicy::Prefilled)` with the operator's input, or
    /// `None` if the operator cancelled (the caller then runs the command with
    /// closed stdin → fast failure + non-interactive remedy footer).
    async fn collect_input_injection(
        &self,
        command: &str,
        prompt: &str,
        secret: bool,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Option<StdinPolicy> {
        let request = InputRequest {
            id: format!("input_{}", uuid::Uuid::new_v4()),
            command: command.to_string(),
            prompt: prompt.to_string(),
            secret,
        };
        let (sender, receiver) = oneshot::channel();
        self.input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .insert(request.id.clone(), sender);
        tracing::info!(%secret, "requesting operator input for interactive command");
        let _ = event_tx.send(AgentEvent::InputRequest(request.clone()));
        match receiver.await.unwrap_or(None) {
            Some(reply) if !reply.text.is_empty() => {
                Some(StdinPolicy::Prefilled { data: reply.text })
            }
            _ => None,
        }
    }

    /// Three-way stdin policy for a `bash` call (L3 + L3.5). See the decision
    /// block in [`Self::execute_tool`] for the contract. `arguments` is the
    /// raw JSON tool arguments.
    async fn decide_bash_stdin(
        &self,
        arguments: &str,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> StdinPolicy {
        // (α) opt-in model stdin: only when the flag is on, which is what
        // dynamically exposes the `stdin` schema field. Read it defensively
        // (absent/invalid → not a model-supplied stdin).
        if self.allow_model_stdin()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments)
            && let Some(data) = v.get("stdin").and_then(|s| s.as_str())
            && !data.is_empty()
        {
            return StdinPolicy::Prefilled {
                data: data.to_string(),
            };
        }
        // (β) human input: classify the command; if interactive, ask the
        // operator. `command` is read from the args (bash's scope_target
        // already extracts it, but re-reading here keeps this self-contained).
        let command = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| {
                v.get("command")
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        if is_interactive_command(&command) {
            let secret = command.split_whitespace().next().map(|t| {
                let prog = t.rsplit('/').next().unwrap_or(t);
                matches!(prog, "sudo" | "su" | "passwd" | "gpg" | "visudo")
                    || prog.starts_with("pinentry")
            }) == Some(true);
            let prompt = if secret {
                "Enter the secret this command is waiting for:".to_string()
            } else {
                format!("This command needs input ({command}):")
            };
            return self
                .collect_input_injection(&command, &prompt, secret, event_tx)
                .await
                .unwrap_or_default();
        }
        // (default) closed hard floor.
        StdinPolicy::default()
    }

    async fn execute_tool(
        &self,
        call: &ToolCall,
        call_id: &str,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let tool: Arc<dyn Tool> = match self
            .resolved_tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| t.name() == call.name)
            .cloned()
            .or_else(|| {
                self.mcp_tools
                    .read()
                    .ok()
                    .and_then(|guard| guard.iter().find(|t| t.name() == call.name).cloned())
            }) {
            Some(t) => t,
            None => {
                return ToolOutput::Error {
                    message: format!("Tool '{}' not found", call.name),
                    detail: None,
                };
            }
        };

        // PreToolUse hooks (ADR-0025): a hook may deny the call before it runs.
        // Arguments are parsed best-effort; an unparseable string still fires
        // the hook with a null input so a guard is never bypassed by bad JSON.
        let tool_input = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or(serde_json::Value::Null);
        if let Some(reason) = self
            .hooks()
            .check_pre_tool_use(
                call.name.as_str(),
                &tool_input,
                &self.hook_session_id(),
                self.hook_cwd().as_deref(),
            )
            .await
        {
            tracing::info!(tool = %call.name, "tool blocked by PreToolUse hook");
            return ToolOutput::Error {
                message: format!("Blocked by hook: {}", reason),
                detail: None,
            };
        }

        // Defense in depth: even though disabled tools are dropped from the
        // schema build, a model that still names one (e.g. from an older
        // turn's tool list still in context) is rejected here rather than
        // silently executed.
        if !self.is_tool_enabled(call.name.as_str()) {
            tracing::warn!(tool = %call.name, "tool disabled this session");
            return ToolOutput::Text(format!(
                "Tool '{}' is disabled for this session. Re-enable it in the Tools modal (/tools).",
                call.name
            ));
        }

        // Operation-scope gate (ADR-0028). The main agent's scope is
        // unrestricted (no-op here); an envoy carries a scope resolved from
        // its profile's `write_paths` / `command_allowlist` grants. Any tool
        // whose [`ScopeTarget`] is a real target (Path/Command) and falls
        // outside the granted scope is blocked outright — a hard capability
        // limit, not a prompt. Sits before the broker, which is the interactive
        // layer inside an unrestricted scope. Tools with
        // [`ScopeTarget::Unspecified`] (`read`, `grep`) skip this gate.
        let target = tool.scope_target(&call.arguments);
        if !matches!(target, neenee_core::ScopeTarget::Unspecified) {
            let scope = self.operation_scope();
            if !scope.allows(&target) {
                tracing::warn!(tool = %call.name, ?scope, "tool blocked by operation scope");
                return ToolOutput::Text(format!(
                    "[operation scope] Tool '{}' is blocked: its target ({:?}) is outside this \
                     agent's permitted scope (granted write paths or command allowlist).",
                    call.name, target
                ));
            }
        }

        if call.name == "ask_user" {
            return self.execute_ask_user(call, call_id, event_tx).await;
        }

        // Permission broker: a tool with a real [`ScopeTarget`] (Path/Command)
        // has a side effect the user should approve. Tools with
        // [`ScopeTarget::Unspecified`] (pure reads/searches) skip the broker.
        if !matches!(target, neenee_core::ScopeTarget::Unspecified) {
            let scope = scope_target_to_rule(&target);
            let rule = PermissionRule {
                tool: tool.name().to_string(),
                scope: scope.clone(),
            };
            let unattended = self.get_unattended();
            let always_allowed = unattended || self.permissions.is_always_allowed(&rule);
            if !always_allowed {
                let request = PermissionRequest {
                    id: format!("permission_{}", uuid::Uuid::new_v4()),
                    tool: tool.name().to_string(),
                    label: tool.permission_label(),
                    description: tool.permission_description(),
                    arguments: call.arguments.clone(),
                    scope,
                };
                let receiver = self.permissions.park_request(request.id.clone());
                tracing::info!(tool = %request.tool, scope = %request.scope, "permission requested");
                let _ = event_tx.send(AgentEvent::PermissionRequest(request.clone()));

                match receiver.await.unwrap_or(PermissionDecision::Reject) {
                    PermissionDecision::Once => {
                        tracing::info!(tool = %tool.name(), decision = "once", "permission granted");
                    }
                    PermissionDecision::Always => {
                        tracing::info!(tool = %tool.name(), decision = "always", "permission granted");
                        self.permissions.add_always(rule);
                    }
                    PermissionDecision::Reject => {
                        tracing::warn!(tool = %tool.name(), "permission denied");
                        return ToolOutput::PermissionDenied {
                            tool: tool.name().to_string(),
                        };
                    }
                }
            } else if unattended {
                tracing::info!(tool = %tool.name(), scope = %scope, "ran unattended");
            }
        }

        // ── Stdin policy decision (L3 + L3.5) ──
        // Decided here, before spawn, for bash only. The three-way decision:
        //   1. opt-in model stdin (α): `allow_model_stdin` on AND the model
        //      supplied a `stdin` arg → Prefilled{model}. Structurally
        //      unreachable unless the flag exposed the schema field.
        //   2. human input (β, default): the interactive classifier matched →
        //      ask the operator; Prefilled{human} or Closed (if cancelled).
        //   3. closed (default hard floor): everything else.
        // For non-bash tools, Closed is always correct (they ignore stdin).
        let stdin_policy = if call.name == "bash" {
            self.decide_bash_stdin(&call.arguments, event_tx).await
        } else {
            StdinPolicy::default()
        };

        // The Envoy / ToolStream events must carry the same id as the
        // up-front ToolCall event (the dispatch-generated `call_id`), not the
        // model's `call.id` — the UI keys its step off the ToolCall event id,
        // so using `call.id` here would orphan every envoy child stream and
        // every live tool stream, leaving the envoy view empty.
        let parent_call_id = call_id.to_string();
        let stream_call_id = call_id.to_string();
        let stream_tx = event_tx.clone();
        let mut on_stream = move |stream: ToolStream| {
            let _ = stream_tx.send(AgentEvent::ToolStream {
                id: stream_call_id.clone(),
                stream,
            });
        };
        match tool
            .call_structured_with_events(
                call_id,
                &call.arguments,
                Box::new(|event| {
                    let _ = event_tx.send(AgentEvent::Envoy {
                        parent_call_id: parent_call_id.clone(),
                        event,
                    });
                }),
                &mut on_stream,
                stdin_policy,
            )
            .await
        {
            Ok(output) => output,
            Err(err) => ToolOutput::Error {
                message: format!("Error executing {}: {}", call.name, err),
                detail: None,
            },
        }
    }

    /// Single-call wrapper that forwards channel events to a mutable callback.
    /// Used by text-fallback paths (one tool call at a time).
    ///
    /// Cancellation-aware: if `cancel` fires while the tool is in flight, the
    /// already-announced call (identified by `call_id`) is paired with a
    /// terminal [`AgentEvent::ToolCancelled`] and this returns
    /// `Err(HarnessError::Interrupted)`.
    pub(crate) async fn execute_tool_evented<F>(
        &self,
        call: &ToolCall,
        call_id: &str,
        cancel: &CancellationToken,
        on_event: &mut F,
    ) -> Result<ToolOutput, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fut = self.execute_tool(call, call_id, &tx);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    while let Ok(event) = rx.try_recv() {
                        on_event(event);
                    }
                    on_event(AgentEvent::ToolCancelled {
                        id: call_id.to_string(),
                        name: call.name.clone(),
                    });
                    return Err(HarnessError::Interrupted);
                }
                event = rx.recv() => {
                    if let Some(event) = event {
                        on_event(event);
                    }
                }
                result = &mut fut => {
                    while let Ok(event) = rx.try_recv() {
                        on_event(event);
                    }
                    return Ok(result);
                }
            }
        }
    }

    /// Execute multiple tool calls concurrently, forwarding interleaved events
    /// to the callback in real time. Returns `(result, duration_ms)` pairs in
    /// the same order as the input calls.
    ///
    /// Cancellation-aware: an interrupt emits a [`AgentEvent::ToolCancelled`]
    /// for every dispatched call id (the whole batch is abandoned — partial
    /// side effects are neither recorded nor replayed by the caller) and
    /// returns `Err(HarnessError::Interrupted)`.
    async fn execute_tools_concurrent<F>(
        &self,
        calls: &[ToolCall],
        call_ids: &[String],
        cancel: &CancellationToken,
        on_event: &mut F,
    ) -> Result<Vec<(ToolOutput, u64)>, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        let (tx, mut rx) = mpsc::unbounded_channel();

        let futs: Vec<_> = calls
            .iter()
            .zip(call_ids.iter())
            .map(|(call, call_id)| {
                let tx = tx.clone();
                let name = call.name.clone();
                let call_id = call_id.to_string();
                async move {
                    let started = std::time::Instant::now();
                    let result = self.execute_tool(call, &call_id, &tx).await;
                    let duration_ms = started.elapsed().as_millis() as u64;
                    // Emit ToolResult immediately through the channel so the TUI
                    // transitions this step from Running to Completed without
                    // waiting for sibling tools to finish. Without this, a
                    // finished envoy task stays "Running" until the slowest
                    // sibling in the batch completes.
                    let output = result.to_text();
                    let _ = tx.send(AgentEvent::ToolResult {
                        id: call_id.clone(),
                        name: name.clone(),
                        output,
                        structured: result.clone(),
                        duration_ms,
                    });
                    (result, duration_ms)
                }
            })
            .collect();

        let all = join_all(futs);
        tokio::pin!(all);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    while let Ok(event) = rx.try_recv() {
                        on_event(event);
                    }
                    for (id, call) in call_ids.iter().zip(calls) {
                        on_event(AgentEvent::ToolCancelled {
                            id: id.clone(),
                            name: call.name.clone(),
                        });
                    }
                    return Err(HarnessError::Interrupted);
                }
                event = rx.recv() => {
                    if let Some(event) = event {
                        on_event(event);
                    }
                }
                results = &mut all => {
                    while let Ok(event) = rx.try_recv() {
                        on_event(event);
                    }
                    return Ok(results);
                }
            }
        }
    }
}

/// Render a [`neenee_core::ScopeTarget`] as the stable string used to key and
/// display a permission rule. A path becomes the path string; a command becomes
/// the command string; [`ScopeTarget::Unspecified`] becomes `"*"` (the legacy
/// "any scope" sentinel), so tools without a locatable target are ruled as
/// before. This string is purely a dedup key + UI label — the actual scope
/// admission decision is made by [`neenee_core::OperationScope::allows`].
fn scope_target_to_rule(target: &neenee_core::ScopeTarget) -> String {
    match target {
        neenee_core::ScopeTarget::Path(p) => p.to_string_lossy().into_owned(),
        neenee_core::ScopeTarget::Command(c) => c.clone(),
        neenee_core::ScopeTarget::Unspecified => "*".to_string(),
    }
}

fn valid_assistant_response(message: &Message) -> bool {
    !message.content.is_empty()
        || message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
        || message
            .reasoning_content
            .as_ref()
            .is_some_and(|reasoning| !reasoning.is_empty())
}

/// Build the "empty assistant response" error, after logging enough state to
/// diagnose why: whether reasoning came through, whether any tool calls were
/// parsed, and which provider/model was responsible. The matching per-turn
/// stream summary (chars fed vs emitted, reasoning/tool-call traffic) is logged
/// by the provider at `neenee_core::provider=debug`.
fn empty_response_error(response: &Message) -> HarnessError {
    tracing::warn!(
        target: "neenee_core::agent",
        provider = ?response.provider,
        model = ?response.model,
        content_chars = response.content.len(),
        reasoning_chars = response
            .reasoning_content
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0),
        tool_calls = response.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0),
        "empty assistant response: provider returned no content and no tool calls",
    );
    HarnessError::Other(
        "Provider returned an empty assistant response (no content, no tool calls).".to_string(),
    )
}

/// Drop assistant messages that carry neither text nor a tool call — the model
/// occasionally emits an empty assistant frame that would otherwise confuse
/// the next provider request. Called from the shared
/// `crate::prompt::Agent::prepare_turn_messages` prep funnel, which both
/// turn loops route through (ADR-0039).
pub(crate) fn remove_empty_assistant_messages(messages: &mut Vec<Message>) {
    messages.retain(|message| message.role != Role::Assistant || valid_assistant_response(message));
}
