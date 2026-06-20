//! System-prompt assembly and skill injection.
//!
//! [`Agent::ensure_system_prompt`] rebuilds the system message each turn from
//! the live mode, goal, tool list and skills index; [`Agent::inject_implicit_skills`]
//! auto-loads skills whose names are mentioned in the latest user turn.

use crate::skills;
use crate::{Agent, AgentMode, GoalStatus, Message, Role};

impl Agent {
    /// Build a system prompt that includes tool definitions and skills index.
    pub(crate) fn build_system_prompt(&self) -> String {
        let mode = self.get_mode();
        let mut parts = vec![
            "You are neenee, an expert AI coding assistant with tool access.".to_string(),
            format!("Current mode: {:?}.", mode),
        ];

        parts.push(
            "Plan workflow: in Build mode, if a request is complex, spans multiple files, has \
             unclear requirements, or would benefit from designing first, call the plan_enter \
             tool to switch to Plan mode. In Plan mode, research with read-only tools, write \
             the plan to .neenee/plans/<name>.md (the only location you may write while \
             planning), then call plan_exit to switch back to Build mode and implement the \
             plan. The user must approve plan_exit before the mode flips, so do not call it \
             until the plan is genuinely ready. Do not enter Plan mode for simple tasks or \
             when the user wants immediate implementation."
                .to_string(),
        );

        // Surface the active plan path in Build mode so the model follows
        // the approved plan without re-reading the file each turn, and tell
        // it about the per-section progress tool.
        if mode == AgentMode::Build {
            if let Some(path) = self.active_plan_path() {
                let display = path.display().to_string();
                parts.push(format!(
                    "You are implementing the approved plan at {display}. Follow it step by \
                     step. If you discover the plan is wrong or incomplete, pause and tell \
                     the user rather than silently deviating. You may re-enter Plan mode by \
                     calling plan_enter if a redesign is required; that clears this plan.\n\n\
                     Use the update_plan_progress tool to mark each `##` section as you work: \
                     in_progress when you start it, done when it is complete. The user sees a \
                     sticky panel with section status, so keeping it current is part of the \
                     job. Before declaring the work complete, spawn an independent verifier \
                     via the `task` tool with a prompt like: 'Re-read the plan at {display}. \
                     Walk through each section and report PASS, PARTIAL, or FAIL with \
                     concrete evidence.' The verifier has a clean context, so it is not \
                     biased by what you wrote.",
                    display = display,
                ));
            }
        }

        if mode == AgentMode::Plan {
            parts.push(
                "You are currently in Plan mode. Plan mode is a read-only phase: you may use \
                 any read-only tool freely (read_file, grep, glob, list_dir, task, etc.) and \
                 you may write files ONLY under .neenee/plans/. Any other write — including \
                 edits to source files, running formatters that rewrite files, or shell \
                 commands whose purpose is to carry out the plan — is blocked and reported \
                 as an error. Tests, builds, and dry-run commands that write only to caches \
                 or build artifacts are allowed.\n\n\
                 Work in three phases:\n\n\
                 Phase 1 — Ground in the environment. Before asking the user anything, run at \
                 least one targeted exploration pass (search files, read entrypoints, \
                 inspect configs/types). Eliminate unknowns you can derive from the repo.\n\n\
                 Phase 2 — Clarify intent. For each remaining unknown, decide whether it is \
                 discoverable (answerable from the repo or system: keep exploring) or a \
                 preference/tradeoff (only answerable by the user: ask via ask_user, with \
                 2–4 concrete options and a recommended default). Bias toward questions over \
                 guessing; never ask about something you could read.\n\n\
                 Phase 3 — Write a decision-complete plan. 'Decision complete' means the \
                 implementer does not need to make any new design choices — only mechanical \
                 execution. Use the plan template: Summary, Key Changes (grouped by \
                 subsystem, not file-by-file), Test Plan, Assumptions. Keep it concise by \
                 default; expand only if the user asks for more detail. Write the plan to \
                 .neenee/plans/<name>.md using the write_file tool — that path is the only \
                 writable target in Plan mode.\n\n\
                 When you present the official plan, wrap the same content in \
                 <proposed_plan>...</proposed_plan> tags (on their own lines; markdown inside) \
                 so the TUI can render it as a distinct card. Emit at most one \
                 <proposed_plan> block per turn, and only when you are presenting a complete \
                 spec. The block is a presentation aid — it does not replace writing the \
                 plan to disk or calling plan_exit.\n\n\
                 When the plan is decision-complete, call plan_exit with the plan_path. The \
                 user will be asked to approve; if they reject, refine the plan based on \
                 their feedback and call plan_exit again. Do NOT use ask_user to ask 'is \
                 this plan okay?' — plan_exit is exactly that approval gate. Do not \
                 implement edits while in Plan mode."
                    .to_string(),
            );
        }

        if let Some(goal) = self.get_goal() {
            parts.push(format!(
                "\nActive harness goal ({:?}):\n{}",
                goal.status, goal.objective
            ));
            if goal.status == GoalStatus::Active {
                if !goal.checklist.is_empty() {
                    parts.push(format!(
                        "Goal checklist:\n{}",
                        goal.checklist
                            .iter()
                            .map(|item| format!("- [{:?}] {}", item.status, item.content))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                parts.push(
                    "Work toward this goal across turns. Use get_goal to read the current goal, \
                     create_goal when the user asks for a new goal, update_goal to mark the goal \
                     complete or blocked, and goal_checklist to expose concrete progress items. \
                     Only when the objective is fully achieved, verified, and every checklist item \
                     is completed or cancelled, call update_goal with status \"complete\"."
                        .to_string(),
                );
            }
        }

        // Tool definitions
        if !self.tools.is_empty() {
            parts.push("\nAvailable tools:".to_string());
            for tool in &self.tools {
                parts.push(format!(
                    "  {} [{:?}]: {}\n    Parameters: {}",
                    tool.name(),
                    tool.access(),
                    tool.description(),
                    tool.parameters()
                ));
            }
            parts.push(
                "\nWhen you need to use a tool, output a JSON object in this exact format:\n\
                 {\"tool\": \"tool_name\", \"arguments\": {...}}\n\
                 Do not ask the user for permission before calling ordinary tools — just call them."
                    .to_string(),
            );
            if self.tools.iter().any(|t| t.name() == "ask_user") {
                parts.push(
                    "\nUse the ask_user tool when you need clarification or a decision from the user: \
                     vague requirements, ambiguous instructions, trade-offs between approaches, \
                     or before risky/destructive actions. Provide 2-4 labeled options per question; \
                     put the recommended option first and suffix its label with '(Recommended)'. \
                     Do NOT use ask_user to ask 'Is this plan okay?' or 'Should I proceed?' — \
                     just take the most reasonable action and mention what you did."
                        .to_string(),
                );
            }
        }

        // Skills index
        let registry = self.skills_registry.lock();
        if !registry.list().is_empty() {
            parts.push(format!(
                "\n{}",
                skills::build_skills_index(&registry.enabled_skills())
            ));
        }

        parts.join("\n")
    }

    /// Inject or update the system message in the message list.
    pub(crate) fn ensure_system_prompt(&self, messages: &mut Vec<Message>) {
        let prompt = self.build_system_prompt();
        if let Some(first) = messages.first_mut() {
            if first.role == Role::System {
                first.content = prompt;
                return;
            }
        }
        messages.insert(0, Message::new(Role::System, prompt));
    }

    /// Auto-load skills whose names are mentioned in the latest user turn.
    /// Mentioned skills are injected as hidden user messages so the model
    /// behaves as if the skill content was explicitly loaded.
    pub(crate) fn inject_implicit_skills(&self, messages: &mut Vec<Message>) {
        let text = messages
            .iter()
            .filter(|m| m.role == Role::User && !m.hidden)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }

        let registry = self.skills_registry.lock();
        let already_loaded: std::collections::HashSet<String> = messages
            .iter()
            .filter(|m| m.role == Role::User && m.hidden)
            .filter_map(|m| {
                let prefix = "[Skill '";
                let start = m.content.find(prefix)? + prefix.len();
                let end = m.content[start..].find("' loaded]")?;
                Some(m.content[start..start + end].to_string())
            })
            .collect();

        for skill in registry.resolve_mentions(&text) {
            if already_loaded.contains(&skill.name) {
                continue;
            }
            messages.push(Message::hidden(
                Role::User,
                format!(
                    "[Skill '{}' loaded]\n{}\n[/Skill]",
                    skill.name, skill.content
                ),
            ));
        }
    }
}
