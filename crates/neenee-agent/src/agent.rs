use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PermissionRule {
    tool: String,
    scope: String,
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
    /// Live plan progress snapshot, parsed from the approved plan markdown
    /// and updated by the `update_plan_progress` tool. Drives the sticky
    /// TUI panel above the input box. Shared with the plan tools.
    plan_progress: Arc<std::sync::Mutex<Option<plan::PlanProgress>>>,
    /// Harness turn counter, bumped at the start of every `execute_turn`.
    /// Shared with `PlanToolContext` and `UpdatePlanProgressTool` so the
    /// latter can stamp `PlanProgress::updated_at_turn` for the TUI stale
    /// detector.
    turn_counter: Arc<std::sync::Mutex<u64>>,
    /// In-memory runtime view of the active goal, used for the checklist.
    goal: Arc<std::sync::Mutex<Option<Goal>>>,
    permissions: std::sync::Mutex<PermissionState>,
    /// When true, write tools execute without a `PermissionRequest`. Set by
    /// `--auto-approve` or `/auto-approve on`. Bypasses the per-call prompt
    /// entirely (the `always` allowlist is also short-circuited because the
    /// prompt block is skipped wholesale).
    auto_approve: Arc<std::sync::Mutex<bool>>,
    ask_user: std::sync::Mutex<AskUserState>,
    pub(crate) skills_registry: skills::SkillRegistry,
    goal_service: GoalService,
    thread_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Context-pressure threshold (in chars) above which the harness asks the
    /// [`CompactionGate`] to relieve pressure between tool rounds. `0` disables
    /// mid-turn relief.
    context_budget_chars: Arc<std::sync::Mutex<usize>>,
    /// Optional mid-turn context-relief gate (see [`CompactionGate`]).
    compaction_gate: Arc<std::sync::Mutex<Option<Arc<dyn CompactionGate>>>>,
}

/// Mutable bookkeeping threaded through a single turn's tool-dispatch rounds.
#[derive(Default)]
struct TurnState {
    token_usage: TokenUsage,
    /// The last tool `(name, arguments)` seen, used to bound consecutive repeats.
    previous_call: Option<(String, String)>,
    repeated_calls: usize,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        mode: AgentMode,
        goal_service: GoalService,
        skills_registry: skills::SkillRegistry,
    ) -> Self {
        let goal = Arc::new(std::sync::Mutex::new(None));
        let thread_id = Arc::new(std::sync::Mutex::new(None));
        let mode = Arc::new(std::sync::Mutex::new(mode));
        let context = goals::tools::GoalToolContext {
            thread_id: Arc::clone(&thread_id),
            goal_service: goal_service.clone(),
        };

        let mut tools = tools;
        tools.retain(|tool| {
            !matches!(
                tool.name(),
                "goal_checklist"
                    | "get_goal"
                    | "create_goal"
                    | "update_goal"
                    | "plan_enter"
                    | "plan_exit"
            )
        });
        tools.push(Arc::new(goals::tools::GoalChecklistTool::new(
            context.clone(),
            Arc::clone(&goal),
        )));
        tools.push(Arc::new(goals::tools::GetGoalTool::new(context.clone())));
        tools.push(Arc::new(goals::tools::CreateGoalTool::new(context.clone())));
        tools.push(Arc::new(goals::tools::UpdateGoalTool::new(context.clone())));

        // Plan-mode workflow tools share the mode handle, the active plan
        // path, and the plan progress snapshot — the same `Arc`s the agent
        // itself holds — so a tool call takes effect immediately and is
        // reflected in the next system prompt and the TUI panel. The turn
        // counter is shared so `update_plan_progress` can stamp
        // `PlanProgress::updated_at_turn` for the stale detector.
        let active_plan_path = Arc::new(std::sync::Mutex::new(None));
        let plan_progress = Arc::new(std::sync::Mutex::new(None));
        let turn_counter = Arc::new(std::sync::Mutex::new(0u64));
        let plan_context = plan::PlanToolContext::shared(
            Arc::clone(&mode),
            Arc::clone(&active_plan_path),
            Arc::clone(&plan_progress),
            Arc::clone(&turn_counter),
        );
        tools.push(Arc::new(plan::PlanEnterTool::new(plan_context.clone())));
        tools.push(Arc::new(plan::PlanExitTool::new(plan_context.clone())));
        tools.push(Arc::new(plan::UpdatePlanProgressTool::new(
            plan_context,
            Arc::clone(&turn_counter),
        )));

        Self {
            provider,
            tools,
            disabled_tools: Arc::new(std::sync::Mutex::new(HashSet::new())),
            mode,
            active_plan_path,
            plan_progress,
            turn_counter,
            goal,
            permissions: std::sync::Mutex::new(PermissionState::default()),
            auto_approve: Arc::new(std::sync::Mutex::new(false)),
            ask_user: std::sync::Mutex::new(AskUserState::default()),
            skills_registry,
            goal_service,
            thread_id,
            context_budget_chars: Arc::new(std::sync::Mutex::new(0)),
            compaction_gate: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Context-pressure threshold (in chars) for mid-turn relief. `0` (the
    /// default) disables the mid-turn [`CompactionGate`].
    pub fn set_context_budget_chars(&self, budget: usize) {
        *self
            .context_budget_chars
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = budget;
    }

    /// Install (or clear with `None`) the mid-turn context-relief gate.
    pub fn set_compaction_gate(&self, gate: Option<Arc<dyn CompactionGate>>) {
        *self
            .compaction_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// Between tool rounds, if context pressure exceeds the configured budget,
    /// hand the live message list to the [`CompactionGate`] for relief (e.g.
    /// pruning old tool results). The gate owns durability of any originals.
    async fn relieve_pressure_if_needed(
        &self,
        messages: &mut Vec<Message>,
        cancel: &CancellationToken,
    ) -> Result<(), HarnessError> {
        let budget = *self
            .context_budget_chars
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if budget == 0 || estimate_chars(messages) <= budget {
            return Ok(());
        }
        let gate = self
            .compaction_gate
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
            self.clear_plan_progress();
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

    /// Current plan progress snapshot, if any. Read by the harness to emit
    /// `AgentEvent::PlanProgressUpdated` and by the TUI to render the sticky
    /// panel.
    pub fn plan_progress(&self) -> Option<plan::PlanProgress> {
        self.plan_progress
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Replace the plan progress snapshot. Used by session-restore paths.
    pub fn set_plan_progress(&self, progress: Option<plan::PlanProgress>) {
        if let Ok(mut guard) = self.plan_progress.lock() {
            *guard = progress;
        }
    }

    /// Drop the plan progress. Called when entering Plan mode so the panel
    /// disappears as soon as the user starts a fresh planning cycle.
    pub fn clear_plan_progress(&self) {
        if let Ok(mut guard) = self.plan_progress.lock() {
            *guard = None;
        }
    }

    /// Current harness turn counter — bumped at the start of every
    /// `execute_turn`. Used by the TUI to detect a stale plan panel (one
    /// whose `updated_at_turn` lags the current turn by more than
    /// `PLAN_STALE_TURN_THRESHOLD`).
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

    pub fn get_goal(&self) -> Option<Goal> {
        self.goal.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_goal(&self, goal: Goal) {
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = Some(goal);
    }

    pub fn restore_goal(&self, goal: Goal) {
        *self.goal.lock().unwrap_or_else(|error| error.into_inner()) = Some(goal);
    }

    pub fn clear_goal(&self) {
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn goal_can_complete(&self) -> bool {
        self.get_goal().is_some_and(|goal| goal.can_complete())
    }

    pub fn goal_service(&self) -> &GoalService {
        &self.goal_service
    }

    pub fn thread_id(&self) -> Option<String> {
        self.thread_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Append a hidden user message that asks the model to continue the active goal.
    pub fn inject_goal_continuation(&self, messages: &mut Vec<Message>) {
        if let Some(goal) = self.get_goal() {
            if goal.status == GoalStatus::Active {
                messages.push(Message::hidden(
                    Role::User,
                    goals::prompts::continuation_prompt(&goal),
                ));
            }
        }
    }

    /// Append a hidden user message that informs the model the goal objective changed.
    pub fn inject_objective_updated(&self, messages: &mut Vec<Message>) {
        if let Some(goal) = self.get_goal() {
            messages.push(Message::hidden(
                Role::User,
                goals::prompts::objective_updated_prompt(&goal),
            ));
        }
    }

    /// Append a hidden user message that informs the model the goal hit its budget.
    pub fn inject_budget_limit(&self, messages: &mut Vec<Message>) {
        if let Some(goal) = self.get_goal() {
            if goal.status == GoalStatus::BudgetLimited {
                messages.push(Message::hidden(
                    Role::User,
                    goals::prompts::budget_limit_prompt(&goal),
                ));
            }
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
    }

    /// Revoke a single cached "always allow" rule. Returns whether a rule was
    /// actually removed (false if the rule was never cached). Powers the
    /// session modal's per-row revoke.
    pub fn revoke_allowed_tool(&self, tool: &str, scope: &str) -> bool {
        let rule = PermissionRule {
            tool: tool.to_string(),
            scope: scope.to_string(),
        };
        self.permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .always
            .remove(&rule)
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
    /// (`builtin` / `mcp:<server>` / `goal` / `plan`) from the tool's name.
    pub fn snapshot_tools(&self) -> Vec<neenee_core::ToolInfo> {
        let disabled = self
            .disabled_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let goal = ["goal_checklist", "get_goal", "create_goal", "update_goal"];
        let plan = ["plan_enter", "plan_exit"];
        let mut infos: Vec<neenee_core::ToolInfo> = self
            .tools
            .iter()
            .map(|t| {
                let name = t.name();
                let source = if let Some(rest) = name.strip_prefix("mcp__") {
                    let server = rest.split("__").next().unwrap_or(rest);
                    format!("mcp:{}", server)
                } else if goal.contains(&name) {
                    "goal".to_string()
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
    /// pane. Mirrors [`RegistryGuard::list`] into the render-friendly DTO.
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
            if tool_rounds >= MAX_TOOL_ROUNDS {
                return Err(HarnessError::Other(format!(
                    "Agent stopped after {} tool rounds. Refine the goal or continue with /loop.",
                    MAX_TOOL_ROUNDS
                )));
            }
            if cancel.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }

            remove_empty_assistant_messages(messages);
            self.ensure_system_prompt(messages);
            self.inject_implicit_skills(messages);

            let response = self.provider.chat(messages.clone()).await?;
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
                self.relieve_pressure_if_needed(messages, cancel).await?;
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
            if tool_rounds >= MAX_TOOL_ROUNDS {
                tracing::warn!(
                    max_rounds = MAX_TOOL_ROUNDS,
                    "turn aborted: tool-round limit"
                );
                return Err(HarnessError::Other(format!(
                    "Agent stopped after {} tool rounds. Refine the goal or continue with /loop.",
                    MAX_TOOL_ROUNDS
                )));
            }
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
            // blocking until the first stream chunk arrives.
            let mut stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
                result = self.provider.stream_chat_events(messages.clone()) => result?,
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
                    event = stream.next() => {
                        let Some(event) = event else { break };
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
                self.relieve_pressure_if_needed(messages, cancel).await?;
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
    /// results, and goal/mode updates — lives in exactly one place.
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
                self.record_tool_result(call, id, &result, duration_ms, messages, state, on_event);
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
                on_event,
            );
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
        self.emit_goal_update(call, on_event);
        self.emit_mode_change(call, on_event);
        self.emit_plan_progress_change(call, on_event);
        on_event(AgentEvent::ToolResult {
            id: call_id.to_string(),
            name: call.name.clone(),
            output: text.clone(),
            structured: result.clone(),
            duration_ms,
        });
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

    pub(crate) fn emit_goal_update<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if call.name == "goal_checklist" {
            if let Some(goal) = self.get_goal() {
                on_event(AgentEvent::GoalUpdated(goal));
            }
        }
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

    /// Notify the harness that the plan progress snapshot changed. Emitted
    /// after `plan_exit` (seeds the panel from the approved plan markdown),
    /// after `plan_enter` (clears it), and after `update_plan_progress`
    /// (mutates a section). The TUI stores the snapshot and re-renders the
    /// sticky panel above the input box.
    fn emit_plan_progress_change<F>(&self, call: &ToolCall, on_event: &mut F)
    where
        F: FnMut(AgentEvent) + Send,
    {
        if matches!(
            call.name.as_str(),
            "plan_enter" | "plan_exit" | "update_plan_progress"
        ) {
            on_event(AgentEvent::PlanProgressUpdated(self.plan_progress()));
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
            Err(err) => ToolOutput::Text(format!("Error executing plan_exit: {}", err)),
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
            None => return ToolOutput::Text(format!("Error: Tool '{}' not found", call.name)),
        };

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

        if tool.access() == ToolAccess::Write {
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
                    description: tool.description().to_string(),
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
            Err(err) => ToolOutput::Text(format!("Error executing {}: {}", call.name, err)),
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
                async move {
                    let started = std::time::Instant::now();
                    let result = self.execute_tool(call, call_id, &tx).await;
                    (result, started.elapsed().as_millis() as u64)
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
