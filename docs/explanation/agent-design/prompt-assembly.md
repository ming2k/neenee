# Prompt and Message Assembly

A model never sees a raw transcript. Every turn the harness composes what the
model actually reads from three independent channels, each with its own rules
for what it carries, when it is rebuilt, and how it reaches the provider. This
page is the integrating view of those channels. The individual mechanisms each
has its own deep-dive; this page ties them together and covers the discipline
that makes the whole assembly auditable.

For the turn that consumes the assembled prompt, see [Harness
architecture](harness.md) and [Turns and rounds](turns-and-rounds.md).

## The three channels

What a model receives on a request is not one prompt but three things traveling
in parallel:

| Channel | What it carries | Rebuilt when | How it reaches the model |
|---------|-----------------|--------------|--------------------------|
| **System** | Identity, neutral behavior, and live context (pursuit, skills catalog) | Every turn, from scratch | A single head system message |
| **User** | Genuine user input, plus harness-injected steering notes | Appended as the turn proceeds | User-role messages |
| **Tools** | Each tool's name, description, and parameter schema | Every turn | The native function-calling `tools` field, outside the conversation |

Keeping the channels separate is the central design idea. The system message is
*recomposed* each turn from live state, so it can never drift stale. The user
channel carries both real input and harness steering, but never the two
confused — every harness insertion is stamped so a persisted transcript can say
exactly what was injected and why. Tools are advertised through the provider's
own schema surface, not described in prose, so the two never contradict.

## The system message

The system message is rebuilt from scratch at the start of every turn, not
stored. It is assembled in a fixed reading order, each section present only when
its precondition holds:

1. **Identity preamble.** Who this agent is — a name and a mission composed into
   one opening sentence. The engine itself is identity-agnostic: it does not
   hardcode a persona or a purpose. The embedding (the CLI, a future frontend)
   supplies them, so the same engine can serve as a coding assistant, a research
   agent, or an operations agent by passing different values. A sub-agent takes
   a third form: its identity *is* its role's full system prompt, injected
   verbatim as the preamble, ignoring name and mission. See
   [Sub-agents](subagents.md).
2. **Neutral behavior.** Mission-independent guidance that applies regardless of
   identity: output tone (concise, direct, no unsolicited recaps), task tracking
   via the todo tools, and how to use `ask_user`. These lines never change with
   persona or mission, which is why they live in the engine rather than the
   embedding.
3. **Active pursuit.** When a session has an active pursuit, its objective is
   inlined into the system message as live context. See [Pursuits](pursuits.md).
4. **Conditional tool guidance.** Tool schemas are declared natively (see
   [Tools](#tools-declared-not-described)), but some guidance exceeds what a
   schema can express. That guidance is injected only when the tool it concerns
   is actually present — for example, the `ask_user` behavioral note appears
   only when `ask_user` is in the tool list.
5. **Skills catalog.** A compact one-line-per-skill index of every enabled
   skill, telling the model what expertise exists without paying for the full
   bodies. See [Skills](skills.md).

The catalog is the only skills content in the system message; skill bodies
reach the model through the user channel on demand. Likewise, tools are never
listed in the system message — their names and schemas travel the dedicated
`tools` field.

Each section is a declarative entry in a prompt registry (`PromptRegistry` in
`neenee-core`), not a hardcoded `push` in an imperative method. A section
carries a stable id, a rank that fixes its reading order, an `is_active`
precondition, and a `render`; the registry composes the active sections in
rank order and stamps the channel's canonical origin. That makes each
section individually unit-testable and individually re-orderable or
disable-able without editing the others. The same engine serves a sub-agent:
its registry is seeded from its profile so the composed system message is the
role's persona plus the neutral sections. See
[ADR-0039](../../adr/0039-unified-prompt-registry.md).

## Conditional injections

The system message is not the only place the harness shapes behavior. As a turn
unfolds across rounds, the harness injects user-role messages under specific
conditions to steer the model. Each injection is a deliberate intervention with
a defined trigger, and each is recorded so the transcript remains faithful.

| Injection | Trigger | Intent |
|-----------|---------|--------|
| **Pursuit continuation** | The `/pursue` stop-gate forces another round because the pursuit is not yet complete | Re-anchor the model on the objective and define what counts as completion; the prompt marks the objective as untrusted user data and sets rigorous completion-audit criteria so the model does not declare victory prematurely |
| **Pursuit objective updated** | The user edits the active pursuit mid-flight | Tell the model the objective changed and to drop work that only served the old one |
| **Read-loop nudge** | The deterministic guard detects a repeated identical read (a stuck anchor or a two-page thrash) | Break the self-reinforcing context: the model is told the repeated read returns identical content and must change course; it escalates once if the loop persists, then stays silent and lets the hard backstops (`Esc`, `hard_stop_rounds`, `abort`) take over |
| **Compaction checkpoint** | Context pressure triggers compaction | Wrap a model-written summary of archived turns under a stable header that flags it as durable context, not a new request. See [Context compaction](context-compaction.md) |
| **Implicit skill** | The latest user message mentions a skill name | Load the skill body so the model behaves as if it had explicitly invoked it. See [Skills](skills.md) |
| **Hook output** | A configured lifecycle hook returns injected context | Let user practice (lint failures, CI gates, reminders) re-enter the conversation. See [Lifecycle hooks](hooks.md) |
| **Sub-agent steering** | A parent agent steers a running child | Land a visible user message directing the sub-agent, or a hidden inter-agent note. See [Sub-agents](subagents.md) |

A defining property is that none of these are semantic guesses. The read-loop
nudge, in particular, fires on *provable* waste — an identical read returns
byte-for-byte identical content — so its detection is pure bookkeeping with no
false positives on legitimate work. Real research reads *different* things, so
its signatures never repeat. This keeps the cheapest intervention also the most
precise. See [ADR-0034](../../adr/0034-range-aware-pruning-and-deterministic-read-loop-guard.md).

The injected prompts follow a consistent design. Pursuit-related prompts wrap
user-supplied text in an XML sentinel (`<objective>` / `<untrusted_objective>`)
and explicitly label it as user data, not higher-priority instructions — a
prompt-injection guard that treats the objective as the task to pursue, never as
authority to override the system message. They also encode fidelity and
completion-audit rules in prose: optimize for movement toward the requested end
state, do not substitute a narrower easier task, and treat completion as
unproven until current evidence proves every requirement.

## The user channel: genuine versus injected

The user channel carries two kinds of message that share a role but are
structurally distinct:

- **Genuine user input** — what the user typed, plus images. This is the real
  conversation.
- **Harness-injected messages** — the steering notes from the table above. They
  are marked **hidden**: they steer the model but are not rendered as part of
  the visible transcript, so the user sees a clean conversation while the model
  receives the guidance it needs.

Because both share `Role::User`, the only reliable way to tell them apart is the
provenance stamp every injection carries. A genuine message has none; an
injected message records both *what* it is and *why* it is here. This is what
makes a persisted transcript reconstructible: resume, replay, and audit can all
answer "what was injected, when, and why" without fragile string-sniffing.

## Tools: declared, not described

Tools are advertised to the model through the provider's native function-calling
surface — the `tools` field alongside the message history — not described in the
system prompt. Each tool declares three things: a name, a description, and a
JSON schema for its parameters. This declaration is request-scoped: every round,
including the round that carries tool results back upstream, re-sends the full
schema set. The provider is stateless across turns.

Two consequences follow from keeping tools out of the prompt:

- **No contradiction.** The model's authoritative source for what a tool is and
  what it accepts is the schema, not a prose paraphrase. The system message
  carries only behavioral guidance the schema cannot express (when to use
  `ask_user`, how to format its options), and only when the tool is present.
- **Dynamic masking.** A tool can be hidden from the model without rebuilding
  the agent: its schema is dropped before declaration and its name is rejected
  at dispatch, but the tool stays installed so it can be re-enabled. The model
  cannot name a tool it was never told about.

MCP servers extend the same surface: their tools are discovered dynamically and
folded into the same declaration path as built-ins. See [MCP
servers](mcp.md).

## Provenance and traceability

The unifying discipline across all three channels is **provenance**. Every
message the harness inserts — a rebuilt system message, a steering note, a
compaction checkpoint, an implicit skill — is stamped at the construction site
with a structured origin that classifies it. Genuine user input, assistant
replies, and tool results carry no origin; only harness injections do.

The classifier is deliberately closed: adding an injection path requires adding
a variant, and exhaustiveness checking forces every injection site to be
stamped. The stamp survives serialization, so a session saved to disk and
reopened later reconstructs the exact live turn. This is the contract that lets
resume, replay, and audit all trust the transcript: nothing was silently
inserted, and everything that was inserted is identifiable.

The same closed classifier does double duty as the registration key for the
system channel's prompt sections: what a transcript records as an injection's
*source* is what the registry knows as a section's *identity*, so provenance
and composition stay in lockstep rather than drifting into two vocabularies.

## Decision history

- [ADR-0039](../../adr/0039-unified-prompt-registry.md) — the system-prompt
  sections become declarative `PromptRegistry` entries keyed by
  `InjectionKind`, replacing the ad-hoc `format!`/`push_str` assembly. The
  same change fixed a latent defect where a sub-agent system message
  pre-seeded before the turn loop was clobbered on round 1.
- [ADR-0034](../../adr/0034-range-aware-pruning-and-deterministic-read-loop-guard.md)
  — the deterministic read-loop guard and why a frequency window replaces a
  consecutive counter.
- [ADR-0030](../../adr/0030-early-loop-intervention-and-round-hook.md) — early
  loop intervention, including the round-hook axis that later fed hook-driven
  injection.
- [ADR-0019](../../adr/0019-model-relative-context-compaction.md) — the
  compaction checkpoint as durable context.

## Adjacent layers

Each injection mechanism has a deep-dive of its own: [Pursuits](pursuits.md)
for the continuation and objective-update prompts, [Context
compaction](context-compaction.md) for the checkpoint, [Skills](skills.md) for
implicit loading, [Lifecycle hooks](hooks.md) for hook-driven context, and
[Sub-agents](subagents.md) for inter-agent steering. The protocol contract that
carries these messages to the provider is covered in [Chat API
primitives](../chat-api-primitives.md) and [Request flow](../request-flow.md).
