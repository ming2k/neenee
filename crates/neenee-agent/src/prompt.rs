//! System-prompt assembly and skill injection.
//!
//! [`Agent::ensure_system_prompt`] rebuilds the system message each turn from
//! the live mode, pursuit, tool list and skills index; [`Agent::inject_implicit_skills`]
//! auto-loads skills whose names are mentioned in the latest user turn.

use crate::skills;
use crate::{Agent, AgentMode, Message, Role};

impl Agent {
    /// Build the system-role message that frames every turn.
    ///
    /// Reassembled from the live mode, pursuit, tool list, and skills index.
    /// The content is bound to [`Role::System`] here, at the construction site,
    /// rather than later at insertion — so the role is traceable from where the
    /// text is assembled, not from a separate function.
    pub(crate) fn build_system_message(&self) -> Message {
        let mode = self.get_mode();
        let mut parts = vec![
            "You are neenee, an expert AI coding assistant with tool access.".to_string(),
            format!("Current mode: {:?}.", mode),
        ];

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
            "Plan workflow: in Build mode, if a request is complex, spans multiple files, or \
             would benefit from designing first, call plan_enter to switch to Plan mode. In \
             Plan mode you research with read-only tools and write the plan to \
             .neenee/plans/<name>.md (the only location you may write while planning), then \
             call plan_exit to switch back and implement it. The user must approve plan_exit \
             before the mode flips, so don't call it until the plan is genuinely ready. \
             Don't enter Plan mode for simple tasks or when the user wants immediate \
             implementation."
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
                     A task list has been seeded from the plan's `##` headings — keep it \
                     current with the `todo` tool (full-replace) or `todo_update` (mark one \
                     step by position or name): move a step to in_progress when you start it \
                     and completed the moment it is done. The user sees a sticky panel with \
                     step status, so keeping it honest is part of the job. Before declaring \
                     the work complete, call verify_plan_execution to spawn an independent \
                     verifier sub-agent with a clean context that re-reads the plan and the \
                     workspace, then reports PASS / PARTIAL / FAIL per section with concrete \
                     evidence. Address every PARTIAL and FAIL before reporting completion to \
                     the user.",
                    display = display,
                ));
            }
        }

        if mode == AgentMode::Plan {
            parts.push(
                "You are currently in Plan mode — a read-only phase. You may use any \
                 read-only tool freely (read_file, grep, glob, list_dir, task, etc.), and \
                 you may write files ONLY under .neenee/plans/. Everything else is blocked \
                 and returned as an error: edits to source files, formatters, and ALL shell \
                 commands — `bash` cannot run in Plan mode at all, so use the dedicated \
                 read-only tools instead of sh/grep/find pipelines.\n\n\
                 Work in three phases:\n\n\
                 1. Ground. Run at least one exploration pass (search, read \
                 entrypoints/configs/types) before asking anything. Resolve every unknown \
                 you can derive from the repo.\n\n\
                 2. Clarify. For each remaining unknown, decide if it is discoverable (keep \
                 exploring) or a preference/tradeoff (ask via ask_user, 2–4 options with a \
                 recommended default). Never ask what you could read.\n\n\
                 3. Write a decision-complete plan. 'Decision complete' = the implementer \
                 makes no new design choices, only executes. Template: Summary, Key Changes \
                 (by subsystem), Test Plan, Assumptions. Keep it concise; expand only if \
                 asked. Write it to .neenee/plans/<name>.md — that path is the only \
                 writable target in Plan mode.\n\n\
                 When the plan is ready, wrap the same content in \
                 <proposed_plan>...</proposed_plan> tags (on their own lines; one block per \
                 turn, only for a complete spec) so the TUI renders it as a card, then call \
                 plan_exit with the plan_path. The user must approve; if they reject, refine \
                 and call it again. Do NOT use ask_user to ask 'is this plan okay?' — \
                 plan_exit is exactly that approval gate. Do not edit source while in Plan \
                 mode."
                    .to_string(),
            );
        }

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
            if !pursuit.is_complete {
                parts.push(
                    "Work toward this pursuit across turns. Use get_pursuit to read the current pursuit, \
                     start_pursuit when the user asks for a new pursuit, and complete_pursuit to mark the \
                     pursuit complete. Only when the objective is fully achieved and verified, call \
                     complete_pursuit with status \"complete\"."
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
