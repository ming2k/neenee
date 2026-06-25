//! System-prompt assembly and skill injection.
//!
//! [`Agent::ensure_system_prompt`] rebuilds the system message each turn from
//! the live mode, pursuit, tool list and skills index; [`Agent::inject_implicit_skills`]
//! auto-loads skills whose names are mentioned in the latest user turn.

use crate::skills;
use crate::{Agent, Message, Role};

impl Agent {
    /// Build the system-role message that frames every turn.
    ///
    /// Reassembled from the live pursuit, tool list, and skills index. The
    /// content is bound to [`Role::System`] here, at the construction site,
    /// rather than later at insertion — so the role is traceable from where the
    /// text is assembled, not from a separate function.
    pub(crate) fn build_system_message(&self) -> Message {
        let mut parts =
            vec!["You are neenee, an expert AI coding assistant with tool access.".to_string()];

        parts.push(
            "Tone and output: be concise and direct. Answer the actual question with the \
             minimum needed — short replies, one word when that suffices — and skip preamble, \
             recaps of what you just did, and unsolicited explanations. Do not add code \
             comments unless asked, and never commit unless explicitly asked. Take the \
             reasonable action with ordinary tools instead of asking permission; reserve \
             questions for genuine ambiguity or trade-offs. Prefer the dedicated tools \
             (read, edit, grep, glob) over shelling out for file work, match existing code \
             style and conventions, and verify with the project's build, tests, or linter \
             when the task implies correctness."
                .to_string(),
        );

        parts.push(
            "Task tracking: for work that spans multiple steps, use the `todo` tool to lay out \
             the steps up front, then update each item's status with `todo_update` (or `todo` for \
             a full restructure) as you progress — move a step to in_progress when you start it \
             and completed/cancelled the moment it is done. Keep the list honest: it is the single \
             source of truth shown to the user, so don't let it drift from reality. At most one \
             item may be in_progress at a time. Skip the list entirely for single-step requests."
                .to_string(),
        );

        if let Some(pursuit) = self.get_pursuit() {
            let state_label = if pursuit.is_complete {
                "complete"
            } else {
                "active"
            };
            parts.push(format!(
                "\nActive harness pursuit ({state_label}):\n{}",
                pursuit.objective
            ));
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

        Message::new(Role::System, parts.join("\n"))
    }

    /// Place the freshly built system message at the head of the conversation,
    /// replacing an existing system message in place or inserting a new one.
    pub(crate) fn ensure_system_prompt(&self, messages: &mut Vec<Message>) {
        let system = self.build_system_message();
        match messages.first_mut() {
            Some(first) if first.role == Role::System => *first = system,
            _ => messages.insert(0, system),
        }
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
