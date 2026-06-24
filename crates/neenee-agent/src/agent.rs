use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
struct PermissionRule {
    tool: String,
    scope: String,
}

/// On-disk shape of the persisted "always allow" allowlist, versioned for
/// future schema evolution. Written by [`Agent::persist_always_permissions`]
/// and read back by [`Agent::load_persistent_permissions`]. Readers reject
/// unknown future versions rather than guessing, so a downgrade silently
/// ignores the file (rather than risking unintended approvals).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedPermissions {
    /// Schema version. Currently `1`; future writers may bump after a
    /// compatible reader is shipped.
    version: u32,
    rules: Vec<PermissionRule>,
}

impl PersistedPermissions {
    /// Version this code writes and reads. Future-compatible readers should
    /// accept any version they understand; unknown versions are ignored.
    const CURRENT_VERSION: u32 = 1;
}

#[derive(Default)]
struct PermissionState {
    always: HashSet<PermissionRule>,
    pending: HashMap<String, oneshot::Sender<PermissionDecision>>,
}

#[derive(Default)]
struct AskUserState {
    pending: HashMap<String, oneshot::Sender<Option<UserQuestionReply>>>,
}

pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    /// Session-level disabled-tool mask. Names here are hidden from the model
    /// (their schemas are dropped before `prepare_tools`) and rejected at
    /// dispatch, but the tool stays installed so it can be re-enabled without
    /// rebuilding the agent. Toggled from the session modal via
    /// `set_tool_enabled` / `ToggleTool`.
    disabled_tools: Arc<std::sync::Mutex<HashSet<String>>>,
    mode: Arc<std::sync::Mutex<AgentMode>>,
    /// Path to the plan file most recently approved via `plan_exit`.
    /// Surfaced in the Build-mode system prompt so the model keeps the plan
    /// in context without re-reading the file each turn. Cleared by
    /// `plan_enter` and `/mode plan`. Shared with the plan tools.
    active_plan_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    /// Unified task list, the single source of truth for "what is left to
    /// do." Drives the sticky panel and persists across restarts. Shared
    /// with the `todo` / `todo_update` tools via `TodoToolContext`, and with
    /// the plan workflow tools via `PlanToolContext` (a plan approved by
    /// `plan_exit` seeds this list; entering Plan mode clears it).
    todos: Arc<std::sync::Mutex<neenee_core::TodoList>>,
    /// Harness turn counter, bumped at the start of every `execute_turn`.
    /// Shared with the plan + todo tools so they can stamp
    /// `updated_at_turn` for the TUI stale detector.
    turn_counter: Arc<std::sync::Mutex<u64>>,
    /// In-memory runtime view of the active pursuit.
    pursuit: Arc<std::sync::Mutex<Option<Pursuit>>>,
    /// Whether a pursuit is armed. When armed, the stop-gate re-injects the
    /// pursuit condition as a hidden user message and forces another model
    /// round instead of ending the turn — until the model signals completion
    /// (emits the completion marker), the safety cap is hit, or the pursuit
    /// is disarmed. This is the `/pursue` mechanism (Claude-Code-style
    /// stop-gate), replacing the old `/loop` outer autonomous loop.
    pursuit_armed: Arc<std::sync::Mutex<bool>>,
    /// Iterations the stop-gate has driven for the current armed pursuit.
    /// Reset to 0 by [`Agent::arm_pursuit`].
    pursuit_iterations: Arc<std::sync::Mutex<u32>>,
    permissions: std::sync::Mutex<PermissionState>,
    /// Optional project root. When set, the `always` permission allowlist is
    /// persisted to `<project_dir>/permissions.json` (see
    /// [`Agent::set_project_root`]) so subsequent sessions in the same project
    /// inherit prior `Always` approvals instead of re-prompting. Best-effort:
    /// I/O failures are logged, never fatal. Sub-agents (TaskTool) and tests
    /// leave this `None` and stay ephemeral.
    project_root: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// When true, write tools execute without a `PermissionRequest`. Set by
    /// `--auto-approve` or `/auto-approve on`. Bypasses the per-call prompt
    /// entirely (the `always` allowlist is also short-circuited because the
    /// prompt block is skipped wholesale).
    auto_approve: Arc<std::sync::Mutex<bool>>,
    ask_user: std::sync::Mutex<AskUserState>,
    pub(crate) skills_registry: skills::SkillRegistry,
    pursuit_service: PursuitService,
    thread_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Context-pressure threshold (in tokens) above which the harness asks the
    /// [`ContextReliefGate`] to relieve pressure between tool rounds. `0` disables
    /// mid-turn relief. Derived from the active model's context window.
    context_prune_threshold_tokens: Arc<std::sync::Mutex<usize>>,
    /// Optional mid-turn context-relief gate (see [`ContextReliefGate`]).
    context_relief_gate: Arc<std::sync::Mutex<Option<Arc<dyn ContextReliefGate>>>>,
    /// Opt-in hard-stop budget (ADR-0018): abort a turn after this many total
    /// tool rounds. Seeded from `Config::agent.hard_stop_rounds` (default `0`
    /// = uncapped, matching ADR-0009) and mutated at runtime via
    /// `set_hard_stop_rounds`. This is the sole execution cap; session review
    /// is on-demand (`/review`) and never aborts a turn.
    hard_stop_rounds: Arc<std::sync::Mutex<usize>>,
    /// Registered review dimensions evaluated by the on-demand diagnostic
    /// sub-agent (`/review`). Defaults to [`crate::default_reviews`] (looping);
    /// empty on sub-agents (which have no `/review` path).
    reviews: Vec<Arc<dyn SessionReview>>,
    /// Whether the verify hard-nudge gate is active. Seeded to `true` and
    /// mutated at runtime via `set_verify_nudge_enabled`. When `false` the
    /// harness never injects the "you forgot to call verify_plan_execution"
    /// reminder, even with an active plan in Build mode.
    verify_nudge_enabled: Arc<std::sync::Mutex<bool>>,
    /// Lifecycle event hooks (ADR-0025). Installed once at startup from the
    /// `[hooks]` config by the CLI; empty by default (sub-agents, tests). Read
    /// at the PreToolUse / PostToolUse / Stop insertion points. Held as a
    /// swappable `Arc` behind a `Mutex` so [`Agent::set_hooks`] can replace the
    /// whole registry without the insertion points holding the lock across the
    /// async `fire` — they clone the `Arc` and drop the guard first.
    hooks: std::sync::Mutex<Arc<crate::hooks::HookRegistry>>,
}

/// Mutable bookkeeping threaded through a single turn's tool-dispatch rounds.
#[derive(Default)]
pub(crate) struct TurnState {
    token_usage: TokenUsage,
    /// The last tool `(name, arguments)` seen, used to bound consecutive repeats.
    previous_call: Option<(String, String)>,
    repeated_calls: usize,
    /// True once the model has invoked `verify_plan_execution` at any
    /// point during this turn. Drives the verify-nudge gate at turn end.
    pub(crate) verify_called_this_turn: bool,
    /// One-shot flag so the verify nudge fires at most once per turn.
    /// Without this the harness and model could ping-pong indefinitely
    /// (nudge → text-only reply → nudge → …).
    pub(crate) verify_nudged: bool,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        mode: AgentMode,
        pursuit_service: PursuitService,
        skills_registry: skills::SkillRegistry,
    ) -> Self {
        // Clone the provider + tool handles before they move into Self, so
        // the VerifyPlanExecutionTool can construct its own internal TaskTool
        // for spawning clean-context verifier sub-agents. The verifier runs
        // in a clean context, so we snapshot the input toolset before the
        // pursuit/plan tools are layered on below; admission (read-only /
        // non-interactive / non-recursive) is applied by the explore profile
        // inside that TaskTool, not re-implemented here. See ADR-0011.
        let verify_provider = provider.clone();
        let verify_tools: Vec<Arc<dyn Tool>> = tools.clone();

        let pursuit = Arc::new(std::sync::Mutex::new(None));
        let thread_id = Arc::new(std::sync::Mutex::new(None));
        let mode = Arc::new(std::sync::Mutex::new(mode));
        let context = pursuits::tools::PursuitToolContext {
            thread_id: Arc::clone(&thread_id),
            pursuit_service: pursuit_service.clone(),
        };

        let mut tools = tools;
        tools.retain(|tool| {
            !matches!(
                tool.name(),
                "get_pursuit" | "start_pursuit" | "complete_pursuit" | "plan_enter" | "plan_exit"
            )
        });
        tools.push(Arc::new(pursuits::tools::GetPursuitTool::new(
            context.clone(),
        )));
        tools.push(Arc::new(pursuits::tools::StartPursuitTool::new(
            context.clone(),
        )));
        tools.push(Arc::new(pursuits::tools::CompletePursuitTool::new(
            context.clone(),
        )));

        // The unified task list shares its cell + turn counter with the plan
        // workflow tools, so a plan transition (plan_exit seeds, plan_enter
        // clears) and an ad-hoc task edit (todo / todo_update) move one
        // shared list. The tools mutate the shared cell, so a call is visible
        // to the next system prompt and the TUI immediately.
        let active_plan_path = Arc::new(std::sync::Mutex::new(None));
        let turn_counter = Arc::new(std::sync::Mutex::new(0u64));
        let todos = Arc::new(std::sync::Mutex::new(neenee_core::TodoList::default()));
        let todo_context =
            neenee_core::TodoToolContext::shared(Arc::clone(&todos), Arc::clone(&turn_counter));
        let plan_context = plan::PlanToolContext::shared(
            Arc::clone(&mode),
            Arc::clone(&active_plan_path),
            todo_context.clone(),
        );
        tools.push(Arc::new(plan::PlanEnterTool::new(plan_context.clone())));
        tools.push(Arc::new(plan::PlanExitTool::new(plan_context.clone())));
        tools.push(Arc::new(crate::plan_verify::VerifyPlanExecutionTool::new(
            verify_provider,
            verify_tools,
            plan_context,
        )));
        tools.push(Arc::new(neenee_core::TodoWriteTool::new(
            todo_context.clone(),
        )));
        tools.push(Arc::new(neenee_core::TodoUpdateTool::new(todo_context)));

        Self {
            provider,
            tools,
            disabled_tools: Arc::new(std::sync::Mutex::new(HashSet::new())),
            mode,
            active_plan_path,
            todos,
            turn_counter,
            pursuit,
            pursuit_armed: Arc::new(std::sync::Mutex::new(false)),
            pursuit_iterations: Arc::new(std::sync::Mutex::new(0)),
            permissions: std::sync::Mutex::new(PermissionState::default()),
            project_root: std::sync::Mutex::new(None),
            auto_approve: Arc::new(std::sync::Mutex::new(false)),
            ask_user: std::sync::Mutex::new(AskUserState::default()),
            skills_registry,
            pursuit_service,
            thread_id,
            context_prune_threshold_tokens: Arc::new(std::sync::Mutex::new(0)),
            context_relief_gate: Arc::new(std::sync::Mutex::new(None)),
            hard_stop_rounds: Arc::new(std::sync::Mutex::new(0)),
            reviews: crate::default_reviews(),
            verify_nudge_enabled: Arc::new(std::sync::Mutex::new(true)),
            hooks: std::sync::Mutex::new(Arc::new(crate::hooks::HookRegistry::empty())),
        }
    }

    /// Context-pressure threshold (in tokens) for mid-turn relief. `0` (the
    /// default) disables the mid-turn [`ContextReliefGate`]. Re-seed on provider
    /// switch so the threshold tracks the new model's context window.
    pub fn set_context_prune_threshold(&self, budget_tokens: usize) {
        *self
            .context_prune_threshold_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = budget_tokens;
    }

    /// Override the opt-in hard-stop budget. Mirrors `[agent] hard_stop_rounds`
    /// in `config.toml` but can be flipped at runtime. `0` (the default) leaves
    /// the turn uncapped, matching ADR-0009. The reviewer sub-agent gets a
    /// tight non-zero bound so a runaway diagnostic cannot loop.
    pub fn set_hard_stop_rounds(&self, rounds: usize) {
        *self
            .hard_stop_rounds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = rounds;
    }

    /// Current hard-stop budget. Read by the `/hard-stop` slash command (if
    /// present) and by `check_hard_stop` each round.
    pub fn get_hard_stop_rounds(&self) -> usize {
        *self
            .hard_stop_rounds
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

    /// Override whether the verify hard-nudge gate is active. Mirrors
    /// `[agent] verify_nudge_enabled` in `config.toml` but can be flipped
    /// at runtime — useful for tests and for headless runs that do not
    /// want a hidden reminder pushed into the transcript.
    pub fn set_verify_nudge_enabled(&self, enabled: bool) {
        *self
            .verify_nudge_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Whether the verify hard-nudge gate is currently active. Read by
    /// the `/verify-nudge` slash command to display the live state.
    pub fn get_verify_nudge_enabled(&self) -> bool {
        *self
            .verify_nudge_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Install (or clear with `None`) the mid-turn context-relief gate.
    pub fn set_context_relief_gate(&self, gate: Option<Arc<dyn ContextReliefGate>>) {
        *self
            .context_relief_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// Install the lifecycle hook registry (ADR-0025). Replaces any prior
    /// registry; intended to be called once at startup after the `[hooks]`
    /// config is parsed. Sub-agents and tests leave the default empty registry.
    pub fn set_hooks(&self, registry: crate::hooks::HookRegistry) {
        *self.hooks.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
    }

    /// Snapshot the hook registry as a cheap `Arc` clone, so insertion points
    /// fire hooks without holding the swap lock across the async `fire`.
    fn hooks(&self) -> Arc<crate::hooks::HookRegistry> {
        self.hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The session id hooks see (the live thread id, if any).
    fn hook_session_id(&self) -> String {
        self.thread_id().unwrap_or_default()
    }

    /// The cwd hooks run under (the persisted project root, if any).
    fn hook_cwd(&self) -> Option<std::path::PathBuf> {
        self.project_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // --- Public hook entry points (ADR-0025) ---------------------------------
    // The PreToolUse / PostToolUse / Stop insertion points are inline in the
    // loop above (they need local control flow); the lifecycle entry points
    // below are called by the driver / orchestration at the session, turn, and
    // compaction boundaries.

    /// `UserPromptSubmit` gate. Called by `execute_turn` before the prompt
    /// enters the transcript: a `Deny` drops it, a `Prepend` prefixes context.
    pub async fn fire_user_prompt_submit(
        &self,
        prompt: &str,
    ) -> crate::hooks::UserPromptVerdict {
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
            .session_start(source, &self.hook_session_id(), self.hook_cwd().as_deref(), messages)
            .await
    }

    /// `SessionEnd` observers. Informational only.
    pub async fn fire_session_end(&self) {
        self.hooks()
            .session_end(&self.hook_session_id(), self.hook_cwd().as_deref())
            .await
    }

    /// Between tool rounds, if context pressure exceeds the configured budget,
    /// hand the live message list to the [`ContextReliefGate`] for relief (e.g.
    /// pruning old tool results). The gate owns durability of any originals.
    async fn relieve_pressure_if_needed(
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
            .context_relief_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(gate) = gate else {
            return Ok(());
        };
        let replacement = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
            replacement = gate.relieve_pressure(messages.clone()) => replacement,
        };
        if let Some(replacement) = replacement {
            if !replacement.is_empty() {
                *messages = replacement;
            }
        }
        Ok(())
    }

    pub fn set_thread_id(&self, thread_id: impl Into<String>) {
        if let Ok(mut guard) = self.thread_id.lock() {
            *guard = Some(thread_id.into());
        }
    }

    pub fn clear_thread_id(&self) {
        if let Ok(mut guard) = self.thread_id.lock() {
            *guard = None;
        }
    }

    pub fn get_mode(&self) -> AgentMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_mode(&self, mode: AgentMode) {
        if let Ok(mut guard) = self.mode.lock() {
            *guard = mode;
        }
        // `/mode plan` is the user-driven equivalent of `plan_enter`:
        // any previously approved plan is no longer the live artifact, so
        // drop it. `/mode build` keeps the existing plan path (if any) so a
        // user who briefly flipped to Plan to peek and then back to Build
        // still sees the right "you are implementing X" hint.
        if mode == AgentMode::Plan {
            self.clear_active_plan_path();
            self.clear_todos();
        }
    }

    /// Path to the plan file most recently approved via `plan_exit`. `None`
    /// when no plan is active (initial state, after re-entering Plan mode).
    pub fn active_plan_path(&self) -> Option<std::path::PathBuf> {
        self.active_plan_path
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Replace the active plan path. Used by session-restore code paths to
    /// rehydrate the hint after resume; in normal operation the plan tools
    /// own this state through their shared `PlanToolContext`.
    pub fn set_active_plan_path(&self, path: Option<std::path::PathBuf>) {
        if let Ok(mut guard) = self.active_plan_path.lock() {
            *guard = path;
        }
    }

    /// Drop the active plan path. Called when entering Plan mode by any path
    /// (tool, slash command, or programmatic) so the Build-mode hint does
    /// not point at a stale plan file.
    pub fn clear_active_plan_path(&self) {
        if let Ok(mut guard) = self.active_plan_path.lock() {
            *guard = None;
        }
    }

    /// Current task list snapshot. Read by the harness to mirror into the
    /// session and by the TUI to render the sticky panel.
    pub fn todos(&self) -> neenee_core::TodoList {
        self.todos.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the task list. Used by `plan_exit` (to seed from the approved
    /// plan), `plan_enter` / `/todos clear` (to clear), and session-restore
    /// paths on resume.
    pub fn set_todos(&self, todos: neenee_core::TodoList) {
        if let Ok(mut guard) = self.todos.lock() {
            *guard = todos;
        }
    }

    /// Drop the task list. Called when entering Plan mode (via `plan_enter`
    /// or `/mode plan`) so the panel disappears as soon as the user starts a
    /// fresh planning cycle.
    pub fn clear_todos(&self) {
        if let Ok(mut guard) = self.todos.lock() {
            *guard = neenee_core::TodoList::default();
        }
    }

    /// Current harness turn counter — bumped at the start of every
    /// `execute_turn`. Used by the TUI to detect a stale task panel (one
    /// whose `updated_at_turn` lags the current turn by more than
    /// `TODO_STALE_TURN_THRESHOLD`).
    pub fn turn_count(&self) -> u64 {
        self.turn_counter.lock().map(|g| *g).unwrap_or(0)
    }

    /// Advance the turn counter. Called once per `execute_turn`. The TUI
    /// reads the resulting value to compute "not updated for N turns".
    pub fn bump_turn(&self) {
        if let Ok(mut g) = self.turn_counter.lock() {
            *g = g.saturating_add(1);
        }
    }

    /// Whether write-tool permission prompts are currently bypassed.
    pub fn get_auto_approve(&self) -> bool {
        *self.auto_approve.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enable or disable auto-approve for this session.
    pub fn set_auto_approve(&self, enabled: bool) {
        *self.auto_approve.lock().unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    pub fn get_pursuit(&self) -> Option<Pursuit> {
        self.pursuit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_pursuit(&self, pursuit: Pursuit) {
        *self.pursuit.lock().unwrap_or_else(|e| e.into_inner()) = Some(pursuit);
    }

    pub fn restore_pursuit(&self, pursuit: Pursuit) {
        *self
            .pursuit
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(pursuit);
    }

    pub fn clear_pursuit(&self) {
        *self.pursuit.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn pursuit_can_complete(&self) -> bool {
        self.get_pursuit().is_some()
    }

    pub fn pursuit_service(&self) -> &PursuitService {
        &self.pursuit_service
    }

    // ── Pursuit stop-gate ───────────────────────────────────────────────
    // `/pursue <condition>` arms the gate. Each time the model would end the
    // turn, the gate re-injects the condition and forces another round until
    // the model signals completion, the safety cap is hit, or the pursuit is
    // disarmed. See [`Agent::pursuit_continuation`].

    /// Arm the pursuit stop-gate and reset the iteration counter.
    pub fn arm_pursuit(&self) {
        *self
            .pursuit_iterations
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = 0;
        *self.pursuit_armed.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Disarm the pursuit stop-gate (e.g. `/pursue clear`).
    pub fn disarm_pursuit(&self) {
        *self.pursuit_armed.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }

    pub fn is_pursuit_armed(&self) -> bool {
        *self.pursuit_armed.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn pursuit_iterations(&self) -> u32 {
        *self
            .pursuit_iterations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Returns a continuation prompt to force another model round, or `None`
    /// to let the turn end. Consulted by both turn loops just before they
    /// return `TurnOutcome`.
    ///
    /// Returns `Some(prompt)` only when: a pursuit is armed, an active
    /// (incomplete) pursuit exists, the latest response did not signal
    /// completion (via the marker), and the iteration cap is not exhausted.
    /// Hitting the cap disarms the pursuit and stops.
    pub(crate) fn pursuit_continuation(&self, response: &Message) -> Option<String> {
        if !self.is_pursuit_armed() {
            return None;
        }
        let pursuit = self.get_pursuit()?;
        if pursuit.is_complete {
            return None;
        }
        // The model signals completion by emitting the marker; if present,
        // let the turn end so orchestration can finalize the pursuit.
        if response.content.contains(crate::PURSUIT_COMPLETE_MARKER) {
            return None;
        }
        let iterations = self.pursuit_iterations();
        if iterations >= MAX_PURSUIT_ITERATIONS {
            // Safety cap: stop driving, disarm so we don't keep re-entering.
            self.disarm_pursuit();
            return None;
        }
        Some(pursuits::prompts::continuation_prompt(&pursuit))
    }

    /// The turn-end gate (ADR-0025). Combines the `/pursue` stop-gate with any
    /// `Stop` hooks: a pursuit forcing continuation wins; otherwise a `Stop`
    /// hook may force another round with feedback. Returns `None` to let the
    /// turn end — i.e. both the pursuit gate and every Stop hook must agree to
    /// stop. The pursuit gate is queried first so its safety-cap disarm side
    /// effect is preserved.
    async fn stop_gate(&self, response: &Message) -> Option<String> {
        if let Some(prompt) = self.pursuit_continuation(response) {
            return Some(prompt);
        }
        self.hooks()
            .check_stop(
                response.content.as_str(),
                &self.hook_session_id(),
                self.hook_cwd().as_deref(),
            )
            .await
    }

    pub fn thread_id(&self) -> Option<String> {
        self.thread_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Append a hidden user message that asks the model to continue the active pursuit.
    pub fn inject_pursuit_continuation(&self, messages: &mut Vec<Message>) {
        if let Some(pursuit) = self.get_pursuit() {
            if !pursuit.is_complete {
                messages.push(Message::hidden(
                    Role::User,
                    pursuits::prompts::continuation_prompt(&pursuit),
                ));
            }
        }
    }

    /// Append a hidden user message that informs the model the pursuit objective changed.
    pub fn inject_objective_updated(&self, messages: &mut Vec<Message>) {
        if let Some(pursuit) = self.get_pursuit() {
            messages.push(Message::hidden(
                Role::User,
                pursuits::prompts::objective_updated_prompt(&pursuit),
            ));
        }
    }

    pub fn reply_permission(&self, request_id: &str, decision: PermissionDecision) -> bool {
        let mut perms = self.permissions.lock().unwrap_or_else(|e| e.into_inner());
        let sender = perms.pending.remove(request_id);
        let sent = sender.is_some_and(|sender| sender.send(decision).is_ok());
        // Rejecting one permission aborts the turn, so resolve every other
        // pending request in the same concurrent batch too. Without this,
        // their tool futures stay blocked on their reply channels and the
        // batch's `join_all` deadlocks — the turn would hang forever.
        if sent && decision == PermissionDecision::Reject {
            for (_, pending_sender) in perms.pending.drain() {
                let _ = pending_sender.send(PermissionDecision::Reject);
            }
        }
        sent
    }

    pub fn reject_pending_permissions(&self) {
        let pending = std::mem::take(
            &mut self
                .permissions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending,
        );
        for (_, sender) in pending {
            let _ = sender.send(PermissionDecision::Reject);
        }
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

    pub fn allowed_tools(&self) -> Vec<String> {
        let mut tools = self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .iter()
            .map(|rule| format!("{} {}", rule.tool, rule.scope))
            .collect::<Vec<_>>();
        tools.sort();
        tools
    }

    pub fn clear_allowed_tools(&self) {
        self.permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .clear();
        self.persist_always_permissions();
    }

    /// Revoke a single cached "always allow" rule. Returns whether a rule was
    /// actually removed (false if the rule was never cached). Powers the
    /// session modal's per-row revoke.
    pub fn revoke_allowed_tool(&self, tool: &str, scope: &str) -> bool {
        let rule = PermissionRule {
            tool: tool.to_string(),
            scope: scope.to_string(),
        };
        let removed = self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .remove(&rule);
        if removed {
            self.persist_always_permissions();
        }
        removed
    }

    /// Structured view of the cached "always allow" rules, for the session
    /// modal's Permissions pane. Unlike [`Agent::allowed_tools`] (which collapses
    /// each rule to a single formatted string), this keeps the tool/scope pair
    /// intact so the modal can target an individual rule for revocation.
    pub fn allowed_tools_structured(&self) -> Vec<neenee_core::PermissionRuleInfo> {
        let mut rules: Vec<neenee_core::PermissionRuleInfo> = self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .iter()
            .map(|rule| neenee_core::PermissionRuleInfo {
                tool: rule.tool.clone(),
                scope: rule.scope.clone(),
            })
            .collect();
        rules.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.scope.cmp(&b.scope)));
        rules
    }

    /// Designate the project whose bucket backs the persistent "always"
    /// allowlist, and load any rules already on disk into the in-memory set.
    /// Pass `None` to disable persistence (sub-agents and most tests do this).
    ///
    /// Loading is best-effort: a missing, unreadable, or unsupported file is
    /// silently ignored — the agent simply starts with an empty allowlist and
    /// re-prompts the user. This is the cross-session hook: a fresh session in
    /// the same project inherits prior `Always` approvals without re-asking.
    pub fn set_project_root(&self, root: Option<std::path::PathBuf>) {
        {
            let mut guard = self.project_root.lock().unwrap_or_else(|e| e.into_inner());
            *guard = root.clone();
        }
        if let Some(root) = root {
            self.load_persistent_permissions(&root);
        }
    }

    /// Read the persisted allowlist (if any) into the in-memory `always` set.
    /// Never fatal — corrupt or missing files just yield an empty allowlist.
    fn load_persistent_permissions(&self, root: &std::path::Path) {
        let path = neenee_store::paths::get().project_permissions(root);
        let Ok(text) = std::fs::read_to_string(&path) else {
            // Common case on a brand-new project; not worth a log line.
            return;
        };
        match serde_json::from_str::<PersistedPermissions>(&text) {
            Ok(persisted) if persisted.version == PersistedPermissions::CURRENT_VERSION => {
                let mut perms = self.permissions.lock().unwrap_or_else(|e| e.into_inner());
                let count = persisted.rules.len();
                for rule in persisted.rules {
                    perms.always.insert(rule);
                }
                tracing::info!(count, path = %path.display(), "loaded persistent permission rules");
            }
            Ok(other) => {
                tracing::warn!(
                    version = other.version,
                    path = %path.display(),
                    "unsupported persisted permissions version; ignoring file",
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not parse persistent permissions file; ignoring",
                );
            }
        }
    }

    /// Atomically mirror the current `always` allowlist into the project
    /// bucket. Best-effort: logs on failure and never propagates the error —
    /// losing a cached approval just means the user gets re-prompted next
    /// session, which is always the safe fallback.
    fn persist_always_permissions(&self) {
        let root = {
            let guard = self.project_root.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let Some(root) = root else {
            return;
        };
        let path = neenee_store::paths::get().project_permissions(&root);
        let snapshot = {
            let perms = self.permissions.lock().unwrap_or_else(|e| e.into_inner());
            let mut rules: Vec<PermissionRule> = perms.always.iter().cloned().collect();
            // Sort for deterministic output — harmless and makes manual
            // inspection / diffs of the on-disk file stable across runs.
            rules.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.scope.cmp(&b.scope)));
            PersistedPermissions {
                version: PersistedPermissions::CURRENT_VERSION,
                rules,
            }
        };
        if let Err(e) = neenee_store::fsutil::atomic_write_json(&path, &snapshot) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not persist permission rules",
            );
        }
    }

    /// Set the session-level enabled flag for a tool. No-op when the name is
    /// unknown (so a stale toggle from the modal cannot poison the dispatch
    /// table). Returns whether the flag actually changed.
    pub fn set_tool_enabled(&self, name: &str, enabled: bool) -> bool {
        let known = self.tools.iter().any(|t| t.name() == name);
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
    fn visible_tools(&self) -> Vec<Arc<dyn Tool>> {
        let disabled = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.tools
            .iter()
            .filter(|t| !disabled.contains(t.name()))
            .cloned()
            .collect()
    }

    /// Structured view of every installed tool, for the session modal's Tools
    /// pane. `enabled` reflects the disabled mask; `source` classifies origin
    /// (`builtin` / `mcp:<server>` / `pursuit` / `plan`) from the tool's name.
    pub fn snapshot_tools(&self) -> Vec<neenee_core::ToolInfo> {
        let disabled = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pursuit = ["get_pursuit", "start_pursuit", "complete_pursuit"];
        let plan = ["plan_enter", "plan_exit"];
        let mut infos: Vec<neenee_core::ToolInfo> = self
            .tools
            .iter()
            .map(|t| {
                let name = t.name();
                let source = if let Some(rest) = name.strip_prefix("mcp__") {
                    let server = rest.split("__").next().unwrap_or(rest);
                    format!("mcp:{}", server)
                } else if pursuit.contains(&name) {
                    "pursuit".to_string()
                } else if plan.contains(&name) {
                    "plan".to_string()
                } else {
                    "builtin".to_string()
                };
                neenee_core::ToolInfo {
                    name: name.to_string(),
                    description: t.description().to_string(),
                    access: t.access(),
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

    pub async fn run(&self, messages: &mut Vec<Message>) -> Result<TurnOutcome, HarnessError> {
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
    ) -> Result<TurnOutcome, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        // Only enabled tools are advertised to the model: the disabled mask is
        // applied here so a toggled-off tool's schema never reaches the
        // provider, which keeps the model from naming it in the first place.
        let visible = self.visible_tools();
        self.provider.prepare_tools(&visible);
        let turn_start = std::time::Instant::now();
        let mut state = TurnState::default();
        let mut tool_rounds = 0;

        loop {
            if cancel.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }

            remove_empty_assistant_messages(messages);
            self.ensure_system_prompt(messages);
            self.inject_implicit_skills(messages);
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
            state.token_usage.total_tokens += pressure::estimate_message_tokens(&response);
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
                self.relieve_pressure_if_needed(messages, cancel).await?;
                continue;
            }

            // Verify hard nudge (mirror of the streaming loop). See the
            // streaming path for the full rationale.
            if self.should_nudge_verify(&state) {
                state.verify_nudged = true;
                messages.push(Message::hidden(
                    Role::User,
                    "You are about to end the turn without calling \
                     verify_plan_execution on the active plan. Before \
                     reporting completion to the user, call \
                     verify_plan_execution so an independent verifier can \
                     audit the implementation against the plan. If you \
                     have already run it this turn and addressed its \
                     findings, ignore this reminder and finish.",
                ));
                continue;
            }

            // Pursuit stop-gate: if a pursuit is armed and the model has not
            // signalled completion, re-inject the condition and force another
            // round instead of ending the turn.
            if let Some(prompt) = self.stop_gate(&response).await {
                *self
                    .pursuit_iterations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) += 1;
                messages.push(Message::hidden(Role::User, prompt));
                continue;
            }

            return Ok(TurnOutcome {
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
    ) -> Result<TurnOutcome, HarnessError>
    where
        F: FnMut(AgentEvent) + Send,
    {
        // Only enabled tools are advertised to the model: the disabled mask is
        // applied here so a toggled-off tool's schema never reaches the
        // provider, which keeps the model from naming it in the first place.
        let visible = self.visible_tools();
        self.provider.prepare_tools(&visible);
        let turn_start = std::time::Instant::now();
        let mut state = TurnState::default();
        let mut tool_rounds = 0;

        loop {
            if cancel.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }

            remove_empty_assistant_messages(messages);
            self.ensure_system_prompt(messages);
            self.inject_implicit_skills(messages);
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
                hidden: false,
                children: None,
                subagent_meta: None,
            };
            if !valid_assistant_response(&response) {
                return Err(empty_response_error(&response));
            }
            state.token_usage.total_tokens += pressure::estimate_message_tokens(&response);
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
                self.relieve_pressure_if_needed(messages, cancel).await?;
                continue;
            }

            // Verify hard nudge: if the model is about to end the turn
            // with an approved plan active but has not run verification
            // yet (and has not already been nudged about it this turn),
            // push a hidden reminder and force one more round. Mirrors
            // the PURSUIT_COMPLETE_MARKER gate but for plan completion, and
            // fires at most once per turn so the model can still wrap
            // up if it judges the nudge irrelevant.
            if self.should_nudge_verify(&state) {
                state.verify_nudged = true;
                messages.push(Message::hidden(
                    Role::User,
                    "You are about to end the turn without calling \
                     verify_plan_execution on the active plan. Before \
                     reporting completion to the user, call \
                     verify_plan_execution so an independent verifier can \
                     audit the implementation against the plan. If you \
                     have already run it this turn and addressed its \
                     findings, ignore this reminder and finish.",
                ));
                continue;
            }

            // Pursuit stop-gate (mirror of the non-streaming path): if a
            // pursuit is armed and the model has not signalled completion,
            // re-inject the condition and force another round.
            if let Some(prompt) = self.stop_gate(&response).await {
                *self
                    .pursuit_iterations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) += 1;
                messages.push(Message::hidden(Role::User, prompt));
                continue;
            }

            return Ok(TurnOutcome {
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
            for call in tool_calls {
                self.guard_repeated_call(
                    call,
                    &mut state.previous_call,
                    &mut state.repeated_calls,
                )?;
            }
            // Track whether the model invoked the plan verifier this round —
            // it feeds the completion-time verify nudge in the outer loop.
            // (Round productivity no longer needs bookkeeping: session review
            // reads the transcript directly, see ADR-0016.)
            if tool_calls
                .iter()
                .any(|call| call.name == "verify_plan_execution")
            {
                state.verify_called_this_turn = true;
            }
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
            // Execute all tool calls concurrently; results arrive in input order.
            // An interrupt converts the whole batch into per-id `ToolCancelled`
            // events — the turn is being aborted, so partial side effects are
            // neither recorded nor replayed (the caller drops the turn history).
            let results = self
                .execute_tools_concurrent(tool_calls, &call_ids, cancel, on_event)
                .await?;
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
            self.guard_repeated_call(&call, &mut state.previous_call, &mut state.repeated_calls)?;
            tracing::debug!(tool = %call.name, "tool call (text fallback)");
            // Mirror the native path's verify-flag tracking so the completion
            // nudge behaves identically for text-emitted tool calls. Round
            // productivity is no longer tracked here (ADR-0016).
            if call.name == "verify_plan_execution" {
                state.verify_called_this_turn = true;
            }
            tool_call::attach_fallback_tool_call(messages, &call);
            let call_id = format!("call_{}", uuid::Uuid::new_v4());
            on_event(AgentEvent::ToolCall {
                id: call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            let result = self
                .execute_tool_evented(&call, &call_id, cancel, on_event)
                .await?;
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
        // Cost attribution: a sub-agent's true token consumption can be 100x
        // the byte-estimate of its final summary, so accumulate the real
        // `TokenUsage` it reported. For every other tool the byte-estimate
        // remains the only signal we have.
        match result.subagent_payload() {
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
        self.emit_mode_change(call, on_event);
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
        // For sub-agent results, attach the nested transcript as `children` on
        // the persisted Tool-role message so resume can rebuild the sub-agent
        // view without a live event stream. The nested `Message`s already
        // self-contain their own tool_calls / tool_call_id / children, so
        // arbitrarily deep sub-agent trees round-trip through session.json.
        // Sidecar `subagent_meta` captures what the live event stream knew but
        // the bare transcript cannot reconstruct on resume: duration, the
        // task description, the toolset size, and an explicit failure flag.
        let tool_message = match result.subagent_payload() {
            Some((sub_messages, _)) => {
                let meta = crate::message::SubagentMeta {
                    duration_ms: Some(duration_ms),
                    failed: result.is_error(),
                    ..Default::default()
                };
                Message::tool_result(call, format!("[{} result]:\n{}", call.name, text))
                    .with_children(sub_messages.to_vec())
                    .with_subagent_meta(meta)
            }
            None => Message::tool_result(call, format!("[{} result]:\n{}", call.name, text)),
        };
        messages.push(tool_message);
    }

    /// Fire PostToolUse (success) or PostToolUseFailure (error) hooks and append
    /// any injected context as hidden user messages (ADR-0025). No-op when the
    /// registry is empty, which is the common case (sub-agents, tests, no
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
        let injected = if result.is_error() {
            registry
                .run_post_tool_use_failure(call.name.as_str(), &summary, &session_id, cwd.as_deref())
                .await
        } else {
            registry
                .run_post_tool_use(call.name.as_str(), &summary, duration_ms, &session_id, cwd.as_deref())
                .await
        };
        for context in injected {
            messages.push(Message::hidden(Role::User, context));
        }
    }

    pub(crate) fn guard_repeated_call(
        &self,
        call: &ToolCall,
        previous_call: &mut Option<(String, String)>,
        repeated_calls: &mut usize,
    ) -> Result<(), HarnessError> {
        let signature = (call.name.clone(), call.arguments.clone());
        if previous_call.as_ref() == Some(&signature) {
            *repeated_calls += 1;
        } else {
            *previous_call = Some(signature);
            *repeated_calls = 1;
        }

        if *repeated_calls > MAX_REPEATED_TOOL_CALLS {
            return Err(HarnessError::Other(format!(
                "Agent stopped after repeating the same '{}' tool call {} times.",
                call.name, MAX_REPEATED_TOOL_CALLS
            )));
        }
        Ok(())
    }

    /// The opt-in hard-stop gate (ADR-0018). Called once per tool round with
    /// the count of rounds that have already run this turn. Returns
    /// `ControlFlow::Break` only when a finite `hard_stop_rounds` budget was
    /// configured and `rounds` has reached it — the caller converts that into
    /// a terminal `HarnessError` via [`Self::hard_stop_error`]. The default
    /// budget (`0`) keeps the turn uncapped, exactly matching ADR-0009.
    ///
    /// Session review no longer fires from the turn loop: it is on-demand via
    /// `/review` ([`Self::review_now`]), which runs the diagnostic sub-agent
    /// against the live transcript and reports a verdict without aborting.
    fn check_hard_stop(&self, rounds: usize) -> std::ops::ControlFlow<()> {
        let budget = self.get_hard_stop_rounds();
        if budget > 0 && rounds >= budget {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    }

    /// Terminal error surfaced when an opt-in `hard_stop_rounds` budget is
    /// exhausted. Echoes the configured budget so the user can tell this apart
    /// from a normal completion in the transcript. The review itself never
    /// produces this — only an explicit user-configured budget does.
    fn hard_stop_error(&self) -> HarnessError {
        let budget = self.get_hard_stop_rounds();
        HarnessError::Other(format!(
            "Agent stopped: the configured hard-stop budget of {budget} tool \
             rounds was reached. This budget is opt-in (`hard_stop_rounds`); \
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
    /// diagnostic sub-agent against `messages` and return one verdict per
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

    /// Whether the harness should fire the verify-hard-nudge gate before
    /// letting the turn end. Conditions: the gate is enabled in config,
    /// there is an active (approved) plan, the agent is in Build mode,
    /// the model has not invoked `verify_plan_execution` at any point
    /// this turn, and the nudge has not already fired (one-shot per
    /// turn). Returns `false` in Plan mode or when no plan is active —
    /// there is nothing to verify.
    pub(crate) fn should_nudge_verify(&self, state: &TurnState) -> bool {
        let enabled = *self
            .verify_nudge_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        enabled
            && !state.verify_nudged
            && !state.verify_called_this_turn
            && self.get_mode() == AgentMode::Build
            && self.active_plan_path().is_some()
    }

    /// Notify the harness that the agent mode changed via `plan_enter` /
    /// `plan_exit`. The tools mutate the shared mode cell themselves; this
    /// only emits the live `ModeChanged` event so the TUI can refresh.
    fn emit_mode_change<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if call.name == "plan_enter" || call.name == "plan_exit" {
            on_event(AgentEvent::ModeChanged(self.get_mode()));
        }
    }

    /// Emit a [`AgentEvent::TodosUpdated`] snapshot whenever a tool mutates
    /// the task list (`todo` full-replace, `todo_update` surgical edit, or a
    /// plan workflow transition that reseeds/clears it). The TUI stores the
    /// snapshot and re-renders the sticky panel above the input box.
    fn emit_todos_change<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if matches!(
            call.name.as_str(),
            "todo" | "todo_update" | "plan_enter" | "plan_exit"
        ) {
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

    /// Run the `plan_exit` tool behind a user-approval gate.
    ///
    /// Builds an `ask_user` request with the plan path (and a short excerpt
    /// of its content, if the file is readable) so the user can confirm the
    /// plan is ready. On approval the underlying `plan_exit` tool runs and
    /// returns the full plan body to the model. On rejection the agent stays
    /// in Plan mode and is asked to refine the plan based on the feedback.
    /// Cancellation is treated as rejection.
    async fn execute_plan_exit(
        &self,
        tool: &Arc<dyn Tool>,
        call: &ToolCall,
        _call_id: &str,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        // Parse plan_path so we can surface it in the confirmation prompt.
        let plan_path = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|value| value.get("plan_path")?.as_str().map(str::to_string))
            .filter(|value| !value.is_empty());

        let excerpt = plan_path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|body| {
                let trimmed = body.trim();
                let chars: Vec<char> = trimmed.chars().take(280).collect();
                let mut snippet: String = chars.into_iter().collect();
                if trimmed.len() > 280 {
                    snippet.push('…');
                }
                snippet
            });

        let header = plan_path.clone().unwrap_or_else(|| "(unsaved)".to_string());
        let question_text = match (&plan_path, &excerpt) {
            (Some(path), Some(snippet)) => format!(
                "Approve this plan and switch to Build mode to start implementing?\n\nPath: {}\n\n{}",
                path, snippet
            ),
            (Some(path), None) => format!(
                "Approve this plan and switch to Build mode to start implementing?\n\nPath: {}\n\n\
                 (The plan file could not be read. Open it to review before approving.)",
                path
            ),
            (None, _) => "Approve exiting Plan mode and switch to Build mode? \
                          (No plan_path was provided.)"
                .to_string(),
        };

        let request = UserQuestionRequest {
            id: format!("plan_exit_{}", uuid::Uuid::new_v4()),
            questions: vec![UserQuestion {
                header: Some(header),
                question: question_text,
                options: vec![
                    UserQuestionOption {
                        label: "Approve".to_string(),
                        description: Some(
                            "Switch to Build mode with full tool access and start implementing."
                                .to_string(),
                        ),
                    },
                    UserQuestionOption {
                        label: "Keep planning".to_string(),
                        description: Some(
                            "Stay in Plan mode. Tell the agent what to refine in your reply."
                                .to_string(),
                        ),
                    },
                ],
                multi_select: false,
            }],
        };

        let (sender, receiver) = oneshot::channel();
        self.ask_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .insert(request.id.clone(), sender);
        tracing::info!("requesting plan_exit approval");
        let _ = event_tx.send(AgentEvent::UserQuestionRequest(request.clone()));

        match receiver.await.unwrap_or(None) {
            Some(reply) => {
                let approved = reply
                    .answers
                    .first()
                    .and_then(|labels| labels.first())
                    .map(|label| label == "Approve")
                    .unwrap_or(false);
                if !approved {
                    tracing::info!("plan_exit rejected by user");
                    return ToolOutput::Text(
                        "User wants to keep planning. Do NOT switch to Build mode yet. \
                         Ask the user (or use the ask_user tool) what should change in the \
                         plan, revise the plan file at .neenee/plans/<name>.md, then call \
                         plan_exit again when it is ready."
                            .to_string(),
                    );
                }
                tracing::info!("plan_exit approved by user");
            }
            None => {
                tracing::info!("plan_exit cancelled by user");
                return ToolOutput::Text(
                    "Plan approval was cancelled. Stay in Plan mode and wait for the \
                     user's next instruction."
                        .to_string(),
                );
            }
        }

        // Approved: delegate to the underlying tool to flip mode, record the
        // plan path, seed plan progress, and return the plan content.
        match tool.call(&call.arguments).await {
            Ok(text) => ToolOutput::Text(text),
            Err(err) => ToolOutput::Error {
                message: format!("Error executing plan_exit: {err}"),
                detail: None,
            },
        }
    }

    async fn execute_tool(
        &self,
        call: &ToolCall,
        call_id: &str,
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let tool = match self.tools.iter().find(|t| t.name() == call.name) {
            Some(t) => t,
            None => {
                return ToolOutput::Error {
                    message: format!("Tool '{}' not found", call.name),
                    detail: None,
                }
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
                "Tool '{}' is disabled for this session. Re-enable it in the session modal (Ctrl+I).",
                call.name
            ));
        }

        if self.get_mode() == AgentMode::Plan && !tool.allowed_in_plan_mode(&call.arguments) {
            tracing::warn!(tool = %call.name, "tool blocked in plan mode");
            return ToolOutput::Text(format!(
                "[Plan mode] Tool '{}' is blocked. Switch to Build mode to execute it.",
                call.name
            ));
        }

        if call.name == "ask_user" {
            return self.execute_ask_user(call, call_id, event_tx).await;
        }

        // `plan_exit` flips the agent out of Plan mode and starts
        // implementation, so the user gets a yes/no confirmation (mirroring
        // opencode's plan_exit and claude-code's ExitPlanMode). Manual
        // `/mode build` skips this — when the user types the slash command
        // they have already decided.
        if call.name == "plan_exit" {
            return self.execute_plan_exit(tool, call, call_id, event_tx).await;
        }

        if tool.access() > ToolAccess::Read {
            let scope = tool.permission_scope(&call.arguments);
            let rule = PermissionRule {
                tool: tool.name().to_string(),
                scope: scope.clone(),
            };
            let auto_approved = self.get_auto_approve();
            let always_allowed = auto_approved
                || self
                    .permissions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .always
                    .contains(&rule);
            if !always_allowed {
                let request = PermissionRequest {
                    id: format!("permission_{}", uuid::Uuid::new_v4()),
                    tool: tool.name().to_string(),
                    label: tool.permission_label(),
                    description: tool.permission_description(),
                    arguments: call.arguments.clone(),
                    scope,
                };
                let (sender, receiver) = oneshot::channel();
                self.permissions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .insert(request.id.clone(), sender);
                tracing::info!(tool = %request.tool, scope = %request.scope, "permission requested");
                let _ = event_tx.send(AgentEvent::PermissionRequest(request.clone()));

                match receiver.await.unwrap_or(PermissionDecision::Reject) {
                    PermissionDecision::Once => {
                        tracing::info!(tool = %tool.name(), decision = "once", "permission granted");
                    }
                    PermissionDecision::Always => {
                        tracing::info!(tool = %tool.name(), decision = "always", "permission granted");
                        self.permissions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .always
                            .insert(rule);
                        self.persist_always_permissions();
                    }
                    PermissionDecision::Reject => {
                        tracing::warn!(tool = %tool.name(), "permission denied");
                        return ToolOutput::PermissionDenied {
                            tool: tool.name().to_string(),
                        };
                    }
                }
            } else if auto_approved {
                tracing::info!(tool = %tool.name(), scope = %scope, "auto-approved");
            }
        }

        // The SubTask / ToolStream events must carry the same id as the
        // up-front ToolCall event (the dispatch-generated `call_id`), not the
        // model's `call.id` — the UI keys its step off the ToolCall event id,
        // so using `call.id` here would orphan every sub-agent child stream and
        // every live tool stream, leaving the sub-agent view empty.
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
                    let _ = event_tx.send(AgentEvent::SubTask {
                        parent_call_id: parent_call_id.clone(),
                        event,
                    });
                }),
                &mut on_stream,
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
                    // finished sub-agent task stays "Running" until the slowest
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

fn remove_empty_assistant_messages(messages: &mut Vec<Message>) {
    messages.retain(|message| message.role != Role::Assistant || valid_assistant_response(message));
}
