# 0039. Unified prompt registry: declarative system/user channels via `PromptSection`

- **Status:** Accepted
- **Date:** 2026-06-27

> **Execution status.** The system-channel registry (primary agent + reviewer)
> is implemented; two latent clobber defects it surfaced are fixed; the two
> duplicated turn-loop prep funnels are collapsed to one. The user channel
> and store-owned prompts were investigated and **deliberately not migrated**
> (templates already centralized, no defect, registry would be ceremony) —
> see the Migration section. The `sections`-on-`SubagentProfile` field is
> deferred until a profile needs a different section set.

## Context

neenee has no prompt abstraction. Every system and user message the harness
constructs is assembled in place with `format!` / `Vec<String>::join` /
`String::push_str` / `include_str!`. There is no `PromptBuilder`,
`PromptSection`, `PromptRegistry`, or `trait Prompt` anywhere in the codebase
(verified by grep across all crates). Adding a new behavioral paragraph to the
system prompt means editing a 70-line imperative method; reordering two
sections means rewriting the method; reasoning about "what sections compose
the subagent's system context" means reading two files. The cost is exactly the
cognitive load the rest of the stack was spared when `Tool`/`Hook`/
`SessionReview`/`SubagentProfile` were introduced as declarative registries.

### The system channel is assembled in six different places

| Site | What it builds | How |
|------|----------------|-----|
| `neenee-agent/src/prompt.rs:17` | Primary agent system message (identity + tone + todo + pursuit + ask_user + skills) | `Vec<String>` of conditional pushes, `join("\n")` at `:83` |
| `neenee-agent/src/session_review.rs:144` | Reviewer subagent system message (REVIEW persona + dimensions + JSON contract) | `String::from(...)` then a chain of `push_str` |
| `neenee-agent/src/session_title.rs:66` | Title subagent system message | `TITLE.system_prompt` used verbatim |
| `neenee-agent/src/subagent_tool.rs:304` | `Task: {description}` system message for the spawned subagent | one-line `format!` |
| `neenee-store/src/session.rs:1642` | Compaction summarizer system message | `SUMMARIZATION_SYSTEM_PROMPT` const (`:1436`) used verbatim |
| `neenee-core/src/subagent.rs:199/225/250/284/339` | Five profile persona strings (`EXPLORE`/`REVIEW`/`TITLE`/`INTERACTIVE`/`QUANT`) | `&'static str` fields on `SubagentProfile` |

These are not six instances of one pattern. They are six unrelated shapes that
happen to produce a `Role::System` message.

### The user channel is assembled in 15+ places

Every harness-injected `Role::User` message is its own `format!`/`Message::injected`
call site: `orchestration.rs:602` (hidden turn), `prompt.rs:127` (implicit skill
load), `pursuit_state.rs:136/150` (pursuit continuation / objective-updated),
`agent.rs:803/810/1103/1343/1632/1672/1696` (steering, stop-gate, hooks,
loop-guard nudge), `hooks.rs:295` (session-start inject),
`session_review.rs:108`, `session_title.rs:67`, `subagent_tool.rs:307`,
`loop_guard.rs:195`, `session.rs:1318` (compaction checkpoint),
`session.rs:1640` (summarization user prompt).

### Two structural defects follow from this

1. **The two turn loops duplicate the prep sequence.** Both
   `agent.rs:1039-1041` (non-streaming) and `agent.rs:1156-1158` (streaming)
   run `remove_empty_assistant_messages` → `ensure_system_prompt` →
   `inject_implicit_skills` before each request. Two copies of the assembly
   entry point; nothing keeps them in sync.

2. **A subagent's system context is split across two files and two mechanisms.**
   `subagent_tool.rs:255` sets the persona via
   `AgentIdentity::from_persona(self.profile.system_prompt)`, which
   `build_system_message` then opens with as the preamble. Separately,
   `subagent_tool.rs:304` pushes a second `Role::System` message carrying
   `Task: {description}`. That second message occupies the index-0 slot that
   `ensure_system_prompt` will replace on the next round (`prompt.rs:92`:
   `Some(first) if first.role == Role::System => *first = system`). So the
   persona framing (from `:255`) and the task framing (from `:306`) compete
   for the same slot rather than compose. The subagent's full system context
   has no single declaration; it emerges from the interaction of two files.

### What is already good, and should be reused

- **`InjectionKind` (`neenee-core/src/message.rs:59-103`)** is a closed enum
  with one variant per injection path (`SystemPrompt`, `PursuitContinuation`,
  `PursuitObjectiveUpdated`, `ImplicitSkill`, `InterAgent`, `SubagentSteer`,
  `LoopReviewNudge`, `CompactionCheckpoint`, `HiddenTurnInput`,
  `Hook(HookEventKind)`). It is currently used *after* the fact — stamped via
  `Message::with_origin` to make a persisted transcript answer "what was
  injected and why". Its variants are exactly the natural keys for a prompt
  registry; the enum is the skeleton already.
- **`neenee-core` is "pure domain, zero I/O" (`lib.rs:7`, ADR-0005)** and
  already hosts the declarative-registry pattern: `Tool`, `Hook`,
  `SessionReview`, `ContextReliefGate` traits, and the `SubagentProfile` /
  `ToolPolicy` value-type bundle. A prompt registry is the same species.
- **`SubagentProfile`** (`subagent.rs:106`) is half a prompt registry already:
  it carries a `system_prompt: &'static str`. Giving it a *list of section ids*
  instead of a single blob is a small, in-character extension.

## Decision

Introduce a single `PromptSection` trait + `PromptRegistry` in `neenee-core`.
Both the **system channel** and the **user channel** route through it. The
`InjectionKind` enum is reused as the registry key — what is stamped as
provenance today becomes the registration id.

### One trait, one channel enum, one registry

```rust
// neenee-core/src/prompt.rs  — pure domain, no IO (ADR-0005)

/// Which message channel a registered section targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptChannel {
    System,
    User,
}

/// A self-contained, declaratively-registered fragment of a prompt.
///
/// One section == one injection path == one `InjectionKind` variant. Sections
/// are unit-ish structs: their `render` draws only from the shared
/// `PromptContext`, so they are individually unit-testable.
pub trait PromptSection: Send + Sync {
    /// Stable id, used for registration, override, and debugging.
    fn id(&self) -> &'static str;
    /// Which channel this section composes into.
    fn channel(&self) -> PromptChannel;
    /// The injection kind this section is stamped with. Reused as the
    /// registry key — `InjectionOrigin::new(self.kind())` is applied by the
    /// registry, not by each call site.
    fn kind(&self) -> InjectionKind;
    /// Default ordering within the channel. Lower sorts earlier. Stable so
    /// insertion order at registration does not leak into the output.
    fn rank(&self) -> u32;
    /// Whether this section applies in the current context. Default `true`.
    /// This is the only branch a section owns — the *decision to appear*,
    /// kept with the *text it would emit*.
    fn is_active(&self, ctx: &PromptContext) -> bool { true }
    /// Render the section body. `None` means "active but produces no text
    /// this turn" (skipped without leaving a blank gap).
    fn render(&self, ctx: &PromptContext) -> Option<String>;
}

/// Read-only view of everything a section may need to render. Plain owned
/// data (no `&Agent`) so the type lives in core without a reverse edge into
/// `neenee-agent`.
pub struct PromptContext<'a> {
    pub identity_preamble: &'a str,
    pub pursuit: Option<&'a Pursuit>,
    pub tool_names: &'a [&'a str],
    pub skills_index: Option<&'a str>,
    pub last_visible_user_text: &'a str,
    // ... grows only when a real section needs a real field
}

pub struct PromptRegistry { system: Vec<Box<dyn PromptSection>>, user: Vec<Box<dyn PromptSection>> }

impl PromptRegistry {
    pub fn new() -> Self;
    pub fn register<S: PromptSection + 'static>(&mut self, section: S);
    /// Move a registered section earlier/later without editing its source.
    /// The lever for "flexible reordering" — default order comes from
    /// `rank()`, overrides come from here.
    pub fn set_rank(&mut self, id: &str, rank: u32);
    pub fn disable(&mut self, id: &str);
    /// Collect active sections for a channel, sort by rank, join with a
    /// blank line, stamp `InjectionOrigin::new(kind)` per-section.
    pub fn build_message(&self, channel: PromptChannel, ctx: &PromptContext) -> Message;
}
```

The primary system prompt at `prompt.rs:17-85` becomes six unit structs:

| Section id | Replaces | `is_active` |
|------------|----------|-------------|
| `system.identity_preamble` | `prompt.rs:23-26` | preamble non-empty |
| `system.tone` | `prompt.rs:28-35` | always |
| `system.todo_guidance` | `prompt.rs:37-45` | always |
| `system.pursuit_objective` | `prompt.rs:47-57` | pursuit present |
| `system.ask_user_guidance` | `prompt.rs:62-72` | `ask_user` in `tool_names` |
| `system.skills_index` | `prompt.rs:74-81` | skills non-empty |

Each renders independently, each has its own test, and "what is in the system
prompt" becomes a registry dump rather than a method read.

### Registration is static, at startup

Application crates (`neenee-code`, `neenee-quant`) build the registry once
where they already build the tool list and identity. This mirrors `ToolFactory`
(`tool_registry.rs`) and the `Hook` registry (ADR-0025): explicit, greppable,
no implicit link-time collection.

```rust
let mut prompts = PromptRegistry::new();
prompts.register(IdentityPreamble);
prompts.register(ToneGuidance);
prompts.register(TodoGuidance);
prompts.register(PursuitObjective);
prompts.register(AskUserGuidance);
prompts.register(SkillsIndex);
// ... user-channel sections
```

### `SubagentProfile` declares its sections by id

`SubagentProfile` gains a `sections: &'static [&'static str]` field. A
subagent's full system context is one declarative list, not the interaction of
`subagent_tool.rs:255` and `:304`. `EXPLORE` becomes:

```rust
pub const EXPLORE: SubagentProfile = SubagentProfile {
    name: "explore",
    sections: &["subagent.persona.explore", "system.tone"],
    // ...
};
```

This collapses the two-mechanism split: the persona is a section (keyed off
the profile), the task framing is a section, ordering is by `rank`, and there
is one assembly path. The `Task: {description}` line at `subagent_tool.rs:304`
becomes a `subagent.task_framing` section that reads `description` from
`PromptContext`, instead of a hand-rolled second system message.

### One `prepare_turn` funnel replaces two duplicated call sites

`ensure_system_prompt` + `inject_implicit_skills` + `remove_empty_assistant_messages`
move into a single `prepare_turn_messages(messages, &registry, &ctx)` called
from both turn loops. The two copies at `agent.rs:1039-1041` and
`agent.rs:1156-1158` collapse to one.

### Layering

- `neenee-core/src/prompt.rs` — `PromptChannel`, `PromptSection`,
  `PromptContext`, `PromptRegistry`. No IO. Re-exports from `lib.rs`.
- `neenee-agent/src/prompt/` — one module per concrete section struct
  (`identity.rs`, `tone.rs`, `todo.rs`, `pursuit.rs`, `ask_user.rs`,
  `skills.rs`, plus the user-channel sections). `build_system_message` and
  `inject_implicit_skills` are deleted; their bodies move into sections.
- `neenee-core/src/subagent.rs` — `SubagentProfile.system_prompt: &'static str`
  is replaced by `sections: &'static [&'static str]`. Persona text moves into
  `subagent.persona.<name>` sections.
- `neenee-store/src/session.rs` — `SUMMARIZATION_SYSTEM_PROMPT` and
  `SUMMARY_TEMPLATE` become a `compaction.summarizer_system` section registered
  by the store-backed compaction path.
- `neenee-code` / `neenee-quant` — build the registry at startup, next to
  tool/identity assembly.

## Alternatives considered

- **`inventory`/`linkme` link-time auto-collection.** Each section self-registers
  at its definition site; no central registration code. Rejected: registration
  becomes implicit and order-dependent (link order leaks into prompt order);
  "what sections compose the system prompt" becomes a repo-wide grep instead of
  one readable list; the existing registries (`ToolFactory`, `Hook`) are
  explicit and this should match them.

- **Config-file-driven section list and ordering.** Sections defined in code,
  but which are enabled and in what order lives in a TOML table. Rejected for
  v1: scope creep. `rank()` defaults plus `PromptRegistry::set_rank`/`disable`
  cover the "flexible reordering" need without a config schema, a validation
  story, or a doc surface. A config layer can be added later behind the same
  registry without touching sections.

- **Keep `InjectionKind` as a post-hoc provenance stamp only.** Section ids
  live under a fresh string namespace, decoupled from the enum. Rejected: the
  enum already enumerates every injection path 1:1; inventing a parallel id
  namespace guarantees drift and doubles the cognitive surface. Reusing the
  enum as the registry key unifies "where this comes from" (provenance) with
  "what this is" (registration).

- **A god-enum of sections instead of a trait.** `enum Section { Identity,
  Tone, Todo, ... }` with a match in one builder. Rejected: a section needs to
  carry render logic and per-section `is_active` branching, both of which the
  trait distributes cleanly; a single match forces every section's text and
  conditions back into one function — the exact structure being removed. The
  trait also lets application crates (`neenee-quant`) register their own
  sections without editing core's enum.

- **A real template engine (Tera/Handlebars/`minijinja`).** Rejected: the only
  templating in the tree is `{{ objective }}` string-replace
  (`pursuits/prompts.rs:30`). Pulling in a template crate to render a handful
  of one-variable sections is unjustified. The trivial-replace stays for the
  two `.md` pursuit templates; everything else is `format!` inside `render`.

- **Fold only the system channel now, defer the user channel.** Rejected: the
  user channel is where the 15+ scattered sites live and where the cognitive
  load is worst. Half a registry is barely better than none, because the
  "where is *this* injection assembled" question would still have two answers
  ("registry" vs "ad hoc"). Both channels land together.

## Consequences

Positive:

- **One真相 per channel.** "What is in the system prompt" and "what user
  messages does the harness inject" are each a registry dump, not a method
  read across six files. Enumerating sections is trivial; adding one is a
  unit struct + one `register` line.
- **The subagent split heals.** A subagent's system context is the section list
  on its profile — persona and task framing compose by `rank`, they do not
  race for the index-0 slot. `subagent_tool.rs:255` and `:304` stop being two
  half-truths.
- **The two turn loops stop diverging.** One `prepare_turn_messages` funnels
  both; `agent.rs:1039-1041` and `:1156-1158` become one call each.
- **Sections are unit-testable in isolation.** Tone guidance, todo guidance,
  ask-user guidance, the loop-guard nudge — each gets its own render test
  without spinning up an `Agent`.
- **Ordering is overridable without source edits.** `set_rank` / `disable`
  give a programmatic lever; a future config layer can drive it without
  re-touching section code.
- **`InjectionKind` gains a second load-bearing use.** It already forced every
  injection to be traceable (exhaustiveness); now it also forces every
  injection to be registered. The discipline compounds.

Negative:

- Migration touches five crates. `prompt.rs` is rewritten; `session_review.rs`,
  `session_title.rs`, `subagent_tool.rs`, `loop_guard.rs`, `pursuit_state.rs`,
  `hooks.rs`, `orchestration.rs`, and `session.rs` each lose their local
  `format!`/`push_str` assembly and gain a section struct instead. The diff is
  large but mechanical.
- `SubagentProfile.system_prompt: &'static str` is a public field; replacing it
  with `sections: &'static [&'static str]` is a breaking change to anyone
  constructing a profile externally. All five built-in profiles are updated
  in-tree; the risk is out-of-tree profiles, which the project does not
  advertise a stability contract for.
- One new trait and one new registry add a concept. The payoff is that six
  existing ad-hoc concepts (six assembly shapes) collapse into it; net
  cognitive load drops, but only after the migration lands.

Neutral:

- `Message`, `InjectionOrigin`, and the `Message::new` / `injected` /
  `with_origin` constructors are unchanged. The registry calls them; it does
  not replace them. Wire format and persistence are untouched.
- Provider adapters (`anthropic_compat.rs`, `gemini.rs`, `openai_compat.rs`)
  are untouched — they consume the assembled `Vec<Message>` as before.
- The two `pursuits/prompts/*.md` templates and the trivial `{{ objective }}`
  replace stay; they are wrapped by sections rather than deleted.

Migration (staged, each step shippable):

1. `neenee-core/src/prompt.rs` — land the trait, channel enum, context, and
   registry with **no** callers. Unit-test the registry in isolation.
2. Migrate the **primary system channel** first: extract the six sections from
   `prompt.rs:17-85`, register them in `neenee-code`, and delete
   `build_system_message`. Replace the two `ensure_system_prompt` call sites
   with one `prepare_turn_messages`. This is the highest-value, most-contained
   cut.
3. Migrate the **subagent system channel**. Executed as the defect-removal
   half: investigation proved the `Task: {description}` system message at
   `subagent_tool.rs:304` was dead code — `ensure_system_prompt` replaces any
   leading system message on round 1, so it was clobbered before the first
   model request and only the persona (also vying for index 0) ever reached
   the model. The line is removed; the subagent now opens with `[User(prompt)]`
   and the registry-built system message (persona via `AgentIdentity` →
   `IdentityPreamble`, plus tone + todo) is inserted at index 0 by the shared
   `prepare_turn_messages` funnel. Locked by
   `subagent_head_system_message_has_no_dead_task_line`. The declarative half
   — adding a `sections: &'static [&'static str]` field to `SubagentProfile`
   so a profile names its composing section ids instead of routing the persona
   through `AgentIdentity` — is deferred to a later stage; it is pure
   declarative sugar at today's profile set (all five compose persona + tone
   + todo to identical output) and carries no defect. It lands when a profile
   genuinely needs a *different* section set.
4. **User channel — investigated, not migrated.** The 15+ harness injection
   sites are each a 1–3 line `Message::injected(Role::User, …, InjectionKind::X)`
   whose content is either a call to an *already-centralized* template
   (`pursuits/prompts.rs` for pursuit continuation / objective-updated,
   `loop_guard::build_nudge` for the read-loop nudge) or a forwarded payload
   (steering notes, hook `Inject` context). The `InjectionKind` enum already
   enumerates every injection path 1:1 — it is the trackability spine the
   system channel needed the registry for. Forcing these through
   `PromptRegistry::render_section` does not fit: a section's `render` draws
   only from `PromptContext`, but user injections carry bespoke payloads (a
   skill's name + body, a hook's context string, a steering note) that would
   require bloating `PromptContext` with per-injection fields or building a
   transient registry per injection. Either undoes the discipline the system
   channel got. The registry's `render_section` / `PromptChannel::User`
   surface stays available for the rare context-derivable user injection;
   the rest remain collocated with their template source. Net: the user
   channel's cognitive load was already lower than the system channel's
   (no 70-line imperative method, templates centralized), so the migration
   is declined rather than churned.
5. **Store-owned prompts — declined.** `SUMMARIZATION_SYSTEM_PROMPT`,
   `SUMMARY_TEMPLATE`, and `CHECKPOINT_HEADER` are already `const`-centralized
   in `session.rs`, and the summarizer is a one-shot `provider.chat(messages)`
   call (no turn loop, so no `ensure_system_prompt` clobber and no per-round
   recomposition to gain from). Folding them into registry sections would
   cross the core←store layer for zero defect payoff and no centralization
   gain over a `const`. The `const`s stay.
6. **Secondary subagent prompts.** `session_title` uses `TITLE.system_prompt`
   verbatim in a single `provider.chat` (no turn loop) — already clean, no
   clobber, left as-is. `session_review` *was* broken by the same clobber
   species as stage 3: the reviewer pre-seeded a system message
   (`build_reviewer_system_prompt`) and then ran `run_streaming_with_events`,
   so `ensure_system_prompt` replaced it on round 1 with the default
   registry's tone+todo — the REVIEW persona, dimension list, and JSON
   contract never reached the model, and the feature limped along only
   because verdict parsing degrades gracefully. Fixed by giving the reviewer
   a dedicated registry (`reviewer_prompt_registry`: `review.persona` +
   `review.dimensions` + `review.json_contract`) installed via
   `Agent::set_prompt_registry`, and starting its transcript at
   `[User(transcript)]` so `ensure_system_prompt` composes the review prompt
   each round. Locked by `reviewer_system_message_carries_persona_dimensions_and_contract`.

## References

- [ADR-0005](0005-strict-layering-and-renames.md) — `neenee-core` is pure
  domain, zero I/O; the registry belongs here alongside `Tool`/`Hook`/
  `SessionReview`/`SubagentProfile`.
- [ADR-0011](0011-subagent-profiles.md) — introduced `SubagentProfile` as a
  declarative bundle; this ADR extends it from one prompt blob to a section
  list.
- [ADR-0025](0025-lifecycle-event-hooks.md) — the precedent for "one trait +
  one registry replaces N one-shot paths" in the same layer. The hook
  registry's static-registration model is reused.
- [ADR-0029](0029-full-duplex-subagent-communication.md) — the
  `AgentOp::InjectUserMessage` / `InterAgentMessage` user-channel injections
  (`agent.rs:803/810`) were a candidate for stage 4; see the migration note
  for why the user channel was ultimately not migrated.
- [ADR-0034](0034-range-aware-pruning-and-deterministic-read-loop-guard.md) —
  the `LoopReviewNudge` injection (`loop_guard.rs:195`) becomes a user-channel
  section.
- `crates/neenee-core/src/message.rs:59-103` — `InjectionKind`, the registry
  key.
- `crates/neenee-agent/src/prompt.rs:17-136` — the primary assembly site being
  decomposed.
- `crates/neenee-agent/src/subagent_tool.rs:255-308` — the subagent split being
  healed.
- `crates/neenee-agent/src/agent.rs:1039-1041` and `:1156-1158` — the duplicated
  prep funnels being collapsed.
