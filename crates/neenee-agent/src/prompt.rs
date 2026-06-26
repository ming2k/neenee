//! System-prompt assembly and skill injection (ADR-0039).
//!
//! The system prompt is no longer one imperative method that pushes string
//! literals into a `Vec`. It is a [`PromptRegistry`] of declarative
//! [`PromptSection`]s — one per behavioral paragraph — registered on the
//! [`Agent`] at construction. [`Agent::ensure_system_prompt`] rebuilds the
//! context from live agent state each round and asks the registry to compose
//! the active system sections in rank order.
//!
//! The six default sections ([`IdentityPreamble`], [`ToneGuidance`],
//! [`TodoGuidance`], [`PursuitObjective`], [`AskUserGuidance`],
//! [`SkillsIndex`]) reproduce the legacy `parts.join("\n")` output byte-for-
//! byte: sections that need a visual gap include a leading `\n` in their own
//! `render`, so joining on a single `\n` preserves the prior layout.
//!
//! [`Agent::inject_implicit_skills`] stays here for now (it is a user-channel
//! injection); ADR-0039 stage 4 will fold it into a user-channel section.

use crate::skills;
use crate::{
    Agent, InjectionKind, InjectionOrigin, Message, PromptChannel, PromptContext, PromptRegistry,
    PromptSection, Role,
};
use neenee_core::{REVIEW, SessionReview};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Default system-channel sections.
//
// Each is a zero-sized struct: the only state a section needs is the live
// turn state, which arrives via [`PromptContext`]. That makes each section
// individually unit-testable and individually re-orderable / disable-able.
// ---------------------------------------------------------------------------

/// Opening identity sentence (name/mission/persona), composed by the
/// embedding. Empty preamble (tests / identity-less agents) → inactive.
struct IdentityPreamble;

impl PromptSection for IdentityPreamble {
    fn id(&self) -> &'static str {
        "system.identity_preamble"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        10
    }
    fn is_active(&self, ctx: &PromptContext) -> bool {
        !ctx.identity_preamble.is_empty()
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        Some(ctx.identity_preamble.clone())
    }
}

/// Mission-neutral tone / output guidance.
struct ToneGuidance;

const TONE: &str = "Tone and output: be concise and direct. Answer the actual question with the \
                    minimum needed — short replies, one word when that suffices — and skip \
                    preamble, recaps of what you just did, and unsolicited explanations. Never \
                    commit unless explicitly asked. Take the reasonable action with ordinary \
                    tools instead of asking permission; reserve questions for genuine ambiguity \
                    or trade-offs.";

impl PromptSection for ToneGuidance {
    fn id(&self) -> &'static str {
        "system.tone"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        20
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(String::from(TONE))
    }
}

/// Task-tracking guidance for the `todo` / `todo_update` tools.
struct TodoGuidance;

const TODO: &str = "Task tracking: for work that spans multiple steps, use the `todo` tool to lay \
                    out the steps up front, then update each item's status with `todo_update` (or \
                    `todo` for a full restructure) as you progress — move a step to in_progress \
                    when you start it and completed/cancelled the moment it is done. Keep the \
                    list honest: it is the single source of truth shown to the user, so don't \
                    let it drift from reality. At most one item may be in_progress at a time. \
                    Skip the list entirely for single-step requests.";

impl PromptSection for TodoGuidance {
    fn id(&self) -> &'static str {
        "system.todo_guidance"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        30
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(String::from(TODO))
    }
}

/// The active pursuit objective, when one is armed. Leading `\n` separates
/// it from the guidance paragraphs above.
struct PursuitObjective;

impl PromptSection for PursuitObjective {
    fn id(&self) -> &'static str {
        "system.pursuit_objective"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        40
    }
    fn is_active(&self, ctx: &PromptContext) -> bool {
        ctx.pursuit.is_some()
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        let pursuit = ctx.pursuit.as_ref()?;
        let state_label = if pursuit.is_complete {
            "complete"
        } else {
            "active"
        };
        Some(format!(
            "\nActive harness pursuit ({state_label}):\n{}",
            pursuit.objective
        ))
    }
}

/// Guidance for the `ask_user` tool, only when that tool is admitted this
/// turn. Leading `\n` separates it from the paragraphs above.
struct AskUserGuidance;

const ASK_USER: &str = "\nUse the ask_user tool when you need clarification or a decision from \
                        the user: vague requirements, ambiguous instructions, trade-offs between \
                        approaches, or before risky/destructive actions. Provide 2-4 labeled \
                        options per question; put the recommended option first and suffix its \
                        label with '(Recommended)'. Do NOT use ask_user to ask 'Is this plan \
                        okay?' or 'Should I proceed?' — just take the most reasonable action and \
                        mention what you did.";

impl PromptSection for AskUserGuidance {
    fn id(&self) -> &'static str {
        "system.ask_user_guidance"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        50
    }
    fn is_active(&self, ctx: &PromptContext) -> bool {
        ctx.tool_names.iter().any(|name| name == "ask_user")
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(String::from(ASK_USER))
    }
}

/// The skills catalog, when any skills are registered. Leading `\n`
/// separates it from the paragraphs above.
struct SkillsIndex;

impl PromptSection for SkillsIndex {
    fn id(&self) -> &'static str {
        "system.skills_index"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        60
    }
    fn is_active(&self, ctx: &PromptContext) -> bool {
        ctx.skills_index.is_some()
    }
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        let index = ctx.skills_index.as_ref()?;
        Some(format!("\n{index}"))
    }
}

/// Build the registry with the default system-channel sections, in rank
/// order. Called once from [`Agent::new`]; an embedding may add more sections
/// (or reorder / disable these) afterwards via the registry handle.
pub(crate) fn default_prompt_registry() -> PromptRegistry {
    let mut registry = PromptRegistry::new();
    registry.register(IdentityPreamble);
    registry.register(ToneGuidance);
    registry.register(TodoGuidance);
    registry.register(PursuitObjective);
    registry.register(AskUserGuidance);
    registry.register(SkillsIndex);
    registry
}

// ---------------------------------------------------------------------------
// Session-review system-channel sections (ADR-0039 stage 6).
//
// The `/review` diagnostic spawns a read-only reviewer subagent that used to
// pre-seed its system message (`build_reviewer_system_prompt`) and then run
// the streaming turn loop. But `ensure_system_prompt` replaces any leading
// system message on round 1, so the seeded persona + dimensions + JSON
// contract were clobbered by the default registry's tone+todo and never
// reached the model — the feature limped along only because verdict parsing
// degrades gracefully. The fix mirrors ADR-0039 stage 3: give the reviewer a
// dedicated registry whose composition IS the review prompt, so the message
// rebuilt every round is correct by construction.
// ---------------------------------------------------------------------------

/// The [`REVIEW`] role framing.
struct ReviewPersona;

impl PromptSection for ReviewPersona {
    fn id(&self) -> &'static str {
        "review.persona"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        10
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(String::from(REVIEW.system_prompt))
    }
}

/// The list of registered review dimensions to evaluate, pre-rendered from
/// the live `[SessionReview]` set. Carried as owned text because the dimension
/// list is bespoke per `/review` run and does not fit the shared
/// [`PromptContext`].
struct ReviewDimensions {
    body: String,
}

impl PromptSection for ReviewDimensions {
    fn id(&self) -> &'static str {
        "review.dimensions"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        20
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(self.body.clone())
    }
}

/// The exact JSON verdict contract the runner parses. Pinned here so prompting
/// and parsing stay in sync.
struct ReviewJsonContract;

const REVIEW_JSON_CONTRACT: &str =
    "Return ONLY a JSON object (no markdown, no prose) of this exact shape:\n\
     {\"verdicts\":[{\"dimension\":\"<id>\",\"status\":\"healthy|watch|stuck\",\
     \"detail\":\"<one short sentence>\"}]}\n\
     Use status \"healthy\" when there is no concern, \"watch\" when progress is \
     slow or risky but not stuck, and \"stuck\" only when the agent is clearly \
     looping without converging. Include one entry per dimension.";

impl PromptSection for ReviewJsonContract {
    fn id(&self) -> &'static str {
        "review.json_contract"
    }
    fn channel(&self) -> PromptChannel {
        PromptChannel::System
    }
    fn kind(&self) -> InjectionKind {
        InjectionKind::SystemPrompt
    }
    fn rank(&self) -> u32 {
        30
    }
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(String::from(REVIEW_JSON_CONTRACT))
    }
}

/// Render the registered dimensions as the bulleted list the reviewer sees
/// between the persona and the JSON contract.
fn render_review_dimensions(dimensions: &[Arc<dyn SessionReview>]) -> String {
    let mut body = String::from(
        "You are evaluating the health of another agent's turn. Assess each of \
         these dimensions:\n\n",
    );
    for dim in dimensions {
        body.push_str(&format!(
            "- `{}` — {}. {}\n",
            dim.id(),
            dim.label(),
            dim.instruction()
        ));
    }
    body
}

/// Build the reviewer subagent's prompt registry: persona + dimensions + JSON
/// contract. Installed on the reviewer via [`Agent::set_prompt_registry`] so
/// its head system message — rebuilt every round — is the review composition.
pub(crate) fn reviewer_prompt_registry(dimensions: &[Arc<dyn SessionReview>]) -> PromptRegistry {
    let mut registry = PromptRegistry::new();
    registry.register(ReviewPersona);
    registry.register(ReviewDimensions {
        body: render_review_dimensions(dimensions),
    });
    registry.register(ReviewJsonContract);
    registry
}

impl Agent {
    /// Derive the read-only prompt context from live agent state. Owned plain
    /// data (ADR-0039): rebuilt each round, no `&Agent` leaks into sections.
    pub(crate) fn build_prompt_context(&self, messages: &[Message]) -> PromptContext {
        let skills_index = {
            let registry = self.skills_registry.lock();
            if registry.list().is_empty() {
                None
            } else {
                Some(skills::build_skills_index(&registry.enabled_skills()))
            }
        };
        let tool_names: Vec<String> = self.tools.iter().map(|t| t.name().to_string()).collect();
        let last_visible_user_text = messages
            .iter()
            .filter(|m| m.role == Role::User && !m.hidden)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        PromptContext {
            identity_preamble: self.identity.preamble(),
            pursuit: self.get_pursuit(),
            tool_names,
            skills_index,
            last_visible_user_text,
        }
    }

    /// Compose the system message from live state and place it at the head of
    /// the conversation, replacing an existing leading system message in place
    /// or inserting a new one.
    pub(crate) fn ensure_system_prompt(&self, messages: &mut Vec<Message>) {
        let ctx = self.build_prompt_context(messages);
        let system = self.prompt_registry.build_system_message(&ctx);
        match messages.first_mut() {
            Some(first) if first.role == Role::System => *first = system,
            _ => messages.insert(0, system),
        }
    }

    /// Single pre-request funnel for both turn loops: drop empty assistant
    /// tails, rebuild the head system message, then auto-load mentioned
    /// skills. Collapses the previously duplicated triple at the two
    /// round-boundary call sites (ADR-0039).
    pub(crate) fn prepare_turn_messages(&self, messages: &mut Vec<Message>) {
        crate::agent::remove_empty_assistant_messages(messages);
        self.ensure_system_prompt(messages);
        self.inject_implicit_skills(messages);
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
            messages.push(Message::injected(
                Role::User,
                format!(
                    "[Skill '{}' loaded]\n{}\n[/Skill]",
                    skill.name, skill.content
                ),
                InjectionOrigin::new(InjectionKind::ImplicitSkill).with_reason(skill.name.clone()),
            ));
        }
    }
}
