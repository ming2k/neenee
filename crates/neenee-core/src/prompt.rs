//! Prompt composition registry (ADR-0039).
//!
//! neenee had no prompt abstraction: every system and user message the harness
//! constructs was assembled in place with `format!` / `Vec<String>::join` /
//! `String::push_str`. This module replaces that with a single declarative
//! registry. One [`PromptSection`] == one injection path == one
//! [`InjectionKind`] variant; the enum that already stamped *provenance* is
//! reused as the registration *key*.
//!
//! The two channels compose differently, and the registry reflects that:
//!
//! - **System channel** — many sections compose into one head `Role::System`
//!   message ([`PromptRegistry::build_system_message`]). Origin is the
//!   channel's canonical [`InjectionKind::SystemPrompt`].
//! - **User channel** — each section is its own injection that fires at a
//!   distinct point in the loop (pursuit continuation, implicit skill,
//!   loop-guard nudge, hooks), so it is rendered one at a time via
//!   [`PromptRegistry::render_section`], stamped with the section's own kind.
//!
//! Registration is static, at startup (mirroring `ToolFactory` and the hook
//! registry of ADR-0025): explicit, greppable, no link-time collection. This
//! module is pure domain, zero I/O (ADR-0005).

use crate::message::{InjectionKind, InjectionOrigin, Message, Role};
use crate::pursuits::Pursuit;

/// Which message channel a registered section composes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptChannel {
    /// Composes into the head `Role::System` message rebuilt every round.
    System,
    /// Composes into a harness-injected `Role::User` message fired at a
    /// specific point in the loop.
    User,
}

impl PromptChannel {
    /// The message role sections of this channel land in.
    pub fn role(self) -> Role {
        match self {
            Self::System => Role::System,
            Self::User => Role::User,
        }
    }
}

/// Read-only view of the live turn state a section may draw on to render.
///
/// Owned plain data (no `&Agent`) so the type lives in core without a reverse
/// dependency edge into `neenee-agent` (ADR-0005 strict layering), and so a
/// section's `render` signature stays free of lifetime parameters. The context
/// is rebuilt each round; the cost of cloning a few small strings is
/// negligible next to a model request. New fields are added only when a real
/// section needs them, so the surface stays minimal.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// The composed identity preamble sentence (name/mission/persona), empty
    /// for tests / when no identity is set.
    pub identity_preamble: String,
    /// The active pursuit, if any.
    pub pursuit: Option<Pursuit>,
    /// Names of the tools admitted this turn (e.g. `["ask_user", ...]`).
    pub tool_names: Vec<String>,
    /// Pre-rendered skills-index string, if any skills are enabled.
    pub skills_index: Option<String>,
    /// Concatenation of the latest visible user text (used by mention-style
    /// sections such as implicit-skill load).
    pub last_visible_user_text: String,
    /// Model-specific guidance from the resolved model. Empty for most
    /// models; non-empty when the model entry carries a
    /// `Model::model_guidance` (e.g. GLM family). Rendered verbatim by
    /// `ModelGuidance`.
    pub model_guidance: &'static str,
}

impl PromptContext {
    /// An all-empty context for registry-mechanics tests and for turns that
    /// genuinely carry no identity / pursuit / tools.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// A self-contained, declaratively-registered fragment of a prompt.
///
/// One section corresponds to exactly one injection path, identified by its
/// [`InjectionKind`]. Sections are typically unit-ish structs whose `render`
/// draws only from the shared [`PromptContext`], which makes each one
/// unit-testable in isolation — the lever the ad-hoc `format!`/`push_str`
/// assembly it replaces did not offer.
///
/// The default [`PromptSection::is_active`] is `true`; a section overrides it
/// to encode the single branch it owns — the *decision to appear* — kept with
/// the *text it would emit*.
pub trait PromptSection: Send + Sync {
    /// Stable id, used for registration, override, disable, and debugging.
    /// Convention: `<channel>.<area>[.<name>]`, e.g. `"system.tone"` or
    /// `"user.pursuit_continuation"`.
    fn id(&self) -> &'static str;
    /// Which channel this section composes into.
    fn channel(&self) -> PromptChannel;
    /// The injection kind this section is stamped with. Reused as the
    /// registry key — what is stamped as provenance today becomes the
    /// registration id.
    fn kind(&self) -> InjectionKind;
    /// Default ordering within the channel. Lower sorts earlier. Stable so
    /// the output never depends on registration call order alone.
    fn rank(&self) -> u32;
    /// Whether this section applies in the current context. Default `true`.
    fn is_active(&self, _ctx: &PromptContext) -> bool {
        true
    }
    /// Render the section body. `None` means "active but produces no text this
    /// turn"; the registry skips a `None` without leaving a blank gap.
    fn render(&self, ctx: &PromptContext) -> Option<String>;
}

/// A registered section plus its runtime overrides.
struct Entry {
    section: Box<dyn PromptSection + Send + Sync>,
    rank_override: Option<u32>,
    disabled: bool,
}

impl Entry {
    fn effective_rank(&self) -> u32 {
        self.rank_override.unwrap_or_else(|| self.section.rank())
    }
}

/// The single entry point for prompt composition (ADR-0039).
///
/// Holds one [`PromptSection`] per injection path, keyed by id. The system
/// channel is assembled into one head message via
/// [`build_system_message`](Self::build_system_message); the user channel is
/// rendered one section at a time via
/// [`render_section`](Self::render_section), since user injections fire at
/// distinct points in the loop.
#[derive(Default)]
pub struct PromptRegistry {
    entries: Vec<Entry>,
}

impl PromptRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a section. Panics on a duplicate id — registration happens at
    /// startup, so a collision is a programmer error, not a runtime condition.
    pub fn register<S: PromptSection + 'static>(&mut self, section: S) {
        let id = section.id();
        assert!(
            !self.entries.iter().any(|e| e.section.id() == id),
            "duplicate PromptSection id: {id}"
        );
        self.entries.push(Entry {
            section: Box::new(section),
            rank_override: None,
            disabled: false,
        });
    }

    /// Override a section's ordering by id, without editing its source. This
    /// is the lever for "flexible reordering": default order comes from
    /// [`PromptSection::rank`], runtime overrides come from here.
    pub fn set_rank(&mut self, id: &str, rank: u32) {
        for entry in &mut self.entries {
            if entry.section.id() == id {
                entry.rank_override = Some(rank);
                return;
            }
        }
        debug_assert!(false, "set_rank: unknown PromptSection id {id}");
    }

    /// Disable a section by id (it is skipped as if inactive). The opposite of
    /// `set_rank` — used to turn a section off without removing its
    /// registration.
    pub fn disable(&mut self, id: &str) {
        for entry in &mut self.entries {
            if entry.section.id() == id {
                entry.disabled = true;
                return;
            }
        }
        debug_assert!(false, "disable: unknown PromptSection id {id}");
    }

    /// Assemble every active **System**-channel section into one head
    /// `Role::System` message: filter by channel + active, sort by effective
    /// rank (stable, so equal ranks preserve registration order), join with a
    /// newline, and stamp [`InjectionKind::SystemPrompt`].
    ///
    /// Sections that need a visual separator include a leading `\n` in their
    /// own `render`, so joining on a single `\n` reproduces the legacy
    /// `parts.join("\n")` layout exactly.
    pub fn build_system_message(&self, ctx: &PromptContext) -> Message {
        let mut active: Vec<(u32, String)> = self
            .entries
            .iter()
            .filter(|e| !e.disabled && e.section.channel() == PromptChannel::System)
            .filter(|e| e.section.is_active(ctx))
            .filter_map(|e| e.section.render(ctx).map(|r| (e.effective_rank(), r)))
            .collect();
        active.sort_by_key(|(rank, _)| *rank);
        let body: String = active
            .into_iter()
            .map(|(_, r)| r)
            .collect::<Vec<_>>()
            .join("\n");
        Message::new(Role::System, body)
            .with_origin(InjectionOrigin::new(InjectionKind::SystemPrompt))
    }

    /// Render a single registered section as its own message, stamped with
    /// that section's own [`InjectionKind`]. This is the path for the user
    /// channel, whose sections fire at distinct loop points rather than all at
    /// once.
    ///
    /// Returns `None` if the id is unknown, the section is disabled or
    /// inactive, or it renders no text.
    ///
    /// The produced message is hidden for the user channel (matching
    /// [`Message::injected`]) and visible for the system channel (matching
    /// [`Message::new`]).
    pub fn render_section(&self, id: &str, ctx: &PromptContext) -> Option<Message> {
        let entry = self.entries.iter().find(|e| e.section.id() == id)?;
        if entry.disabled {
            return None;
        }
        let section = &entry.section;
        if !section.is_active(ctx) {
            return None;
        }
        let text = section.render(ctx)?;
        let origin = InjectionOrigin::new(section.kind());
        Some(match section.channel() {
            PromptChannel::User => Message::injected(section.channel().role(), text, origin),
            PromptChannel::System => {
                Message::new(section.channel().role(), text).with_origin(origin)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configurable section for registry-mechanics tests.
    struct S {
        id: &'static str,
        channel: PromptChannel,
        kind: InjectionKind,
        rank: u32,
        active: bool,
        text: Option<&'static str>,
    }

    impl PromptSection for S {
        fn id(&self) -> &'static str {
            self.id
        }
        fn channel(&self) -> PromptChannel {
            self.channel
        }
        fn kind(&self) -> InjectionKind {
            self.kind
        }
        fn rank(&self) -> u32 {
            self.rank
        }
        fn is_active(&self, _ctx: &PromptContext) -> bool {
            self.active
        }
        fn render(&self, _ctx: &PromptContext) -> Option<String> {
            self.text.map(String::from)
        }
    }

    fn sys(id: &'static str, rank: u32, text: &'static str) -> S {
        S {
            id,
            channel: PromptChannel::System,
            kind: InjectionKind::SystemPrompt,
            rank,
            active: true,
            text: Some(text),
        }
    }

    #[test]
    fn system_message_orders_by_rank_and_joins_with_newline() {
        let mut reg = PromptRegistry::new();
        // Registered out of rank order; output must follow rank.
        reg.register(sys("system.tone", 20, "Tone body."));
        reg.register(sys("system.identity", 10, "Identity body."));
        reg.register(sys("system.todo", 30, "Todo body."));

        let msg = reg.build_system_message(&PromptContext::empty());
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.content, "Identity body.\nTone body.\nTodo body.");
    }

    #[test]
    fn equal_ranks_preserve_registration_order() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.a", 10, "A"));
        reg.register(sys("system.b", 10, "B"));
        reg.register(sys("system.c", 10, "C"));
        let msg = reg.build_system_message(&PromptContext::empty());
        assert_eq!(msg.content, "A\nB\nC");
    }

    #[test]
    fn inactive_and_empty_renders_are_skipped_without_gaps() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.a", 10, "A"));
        reg.register(S {
            id: "system.inactive",
            channel: PromptChannel::System,
            kind: InjectionKind::SystemPrompt,
            rank: 20,
            active: false,
            text: Some("should not appear"),
        });
        reg.register(S {
            id: "system.empty",
            channel: PromptChannel::System,
            kind: InjectionKind::SystemPrompt,
            rank: 30,
            active: true,
            text: None,
        });
        reg.register(sys("system.d", 40, "D"));

        let msg = reg.build_system_message(&PromptContext::empty());
        // No blank line where the skipped sections would have been.
        assert_eq!(msg.content, "A\nD");
    }

    #[test]
    fn user_channel_sections_are_excluded_from_build_system_message() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.tone", 10, "Tone"));
        reg.register(S {
            id: "user.nudge",
            channel: PromptChannel::User,
            kind: InjectionKind::LoopReviewNudge,
            rank: 10,
            active: true,
            text: Some("Nudge"),
        });
        let msg = reg.build_system_message(&PromptContext::empty());
        assert_eq!(msg.content, "Tone");
    }

    #[test]
    fn system_message_origin_is_system_prompt() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.tone", 10, "Tone"));
        let msg = reg.build_system_message(&PromptContext::empty());
        assert_eq!(
            msg.origin.as_ref().map(|o| o.kind),
            Some(InjectionKind::SystemPrompt)
        );
    }

    #[test]
    fn render_section_returns_hidden_user_message_with_section_kind() {
        let mut reg = PromptRegistry::new();
        reg.register(S {
            id: "user.pursuit_continuation",
            channel: PromptChannel::User,
            kind: InjectionKind::PursuitContinuation,
            rank: 10,
            active: true,
            text: Some("Continue the pursuit."),
        });

        let msg = reg
            .render_section("user.pursuit_continuation", &PromptContext::empty())
            .expect("section renders");
        assert_eq!(msg.role, Role::User);
        assert!(msg.hidden, "user-channel section must be hidden");
        assert_eq!(msg.content, "Continue the pursuit.");
        assert_eq!(
            msg.origin.as_ref().map(|o| o.kind),
            Some(InjectionKind::PursuitContinuation),
        );
    }

    #[test]
    fn render_section_unknown_inactive_or_empty_yields_none() {
        let mut reg = PromptRegistry::new();
        reg.register(S {
            id: "user.x",
            channel: PromptChannel::User,
            kind: InjectionKind::ImplicitSkill,
            rank: 10,
            active: false,
            text: Some("X"),
        });
        reg.register(S {
            id: "user.y",
            channel: PromptChannel::User,
            kind: InjectionKind::ImplicitSkill,
            rank: 20,
            active: true,
            text: None,
        });

        assert!(
            reg.render_section("missing", &PromptContext::empty())
                .is_none()
        );
        assert!(
            reg.render_section("user.x", &PromptContext::empty())
                .is_none(),
            "inactive section yields None"
        );
        assert!(
            reg.render_section("user.y", &PromptContext::empty())
                .is_none(),
            "section rendering None yields None"
        );
    }

    #[test]
    fn set_rank_reorders_output() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.a", 10, "A"));
        reg.register(sys("system.b", 20, "B"));
        reg.set_rank("system.b", 5);

        let msg = reg.build_system_message(&PromptContext::empty());
        assert_eq!(msg.content, "B\nA", "override rank wins over default");
    }

    #[test]
    fn disable_removes_section_from_output() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.a", 10, "A"));
        reg.register(sys("system.b", 20, "B"));
        reg.disable("system.b");

        let msg = reg.build_system_message(&PromptContext::empty());
        assert_eq!(msg.content, "A");
        assert!(
            reg.render_section("system.b", &PromptContext::empty())
                .is_none(),
            "disabled section also yields None via render_section"
        );
    }

    #[test]
    #[should_panic(expected = "duplicate PromptSection id: system.tone")]
    fn register_panics_on_duplicate_id() {
        let mut reg = PromptRegistry::new();
        reg.register(sys("system.tone", 10, "Tone"));
        reg.register(sys("system.tone", 20, "Tone again"));
    }
}
