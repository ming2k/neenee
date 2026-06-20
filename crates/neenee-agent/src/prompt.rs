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
            "Plan workflow: in Build mode, if a request is complex, spans multiple files, or would \
             benefit from designing first, call the plan_enter tool to switch to Plan mode. In Plan \
             mode, research with read-only tools, write the plan to .neenee/plans/<name>.md (the \
             only location you may write while planning), then call plan_exit to switch back to \
             Build mode and implement the plan. Do not enter Plan mode for simple tasks or when the \
             user wants immediate implementation."
                .to_string(),
        );

        if mode == AgentMode::Plan {
            parts.push(
                "You are currently in Plan mode. You may only use read-only tools, except that you \
                 may write files under .neenee/plans/. When the plan is written and finalized, call \
                 plan_exit to return to Build mode and implement it; do not implement edits while \
                 in Plan mode."
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
