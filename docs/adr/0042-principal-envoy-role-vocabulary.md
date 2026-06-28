# 0042. Principal / Envoy role vocabulary

- **Status:** Accepted
- **Date:** 2026-06-28

## Context

The codebase used "agent" for two different things at once: the top-level,
human-facing conversation loop, and the isolated children it spawns to research
sub-questions ("subagent"). Because a subagent *is* an agent, the two terms
collided in prose, type names, and the planned multi-agent group chat (several
top-level agents sharing one conversation, each able to spawn children). The
overload made it ambiguous which "agent" any given identifier meant.

The execution engine is a single struct, `Agent`
(`crates/neenee-agent/src/agent.rs`): `EnvoyTool::run_envoy`
(`crates/neenee-agent/src/envoy_tool.rs`) builds another `Agent` to run a
child, and a child's native `AgentEvent`s are wrapped into `EnvoyEvent` by
`forward_event`. So "agent" is genuinely the engine; the distinction worth
naming is the *role* an `Agent` instance runs in.

## Decision

Adopt a three-tier vocabulary across the whole project:

1. **`agent` — the abstract layer (umbrella).** Keep for the execution engine
   and engine-level protocol that any role shares: the `Agent` struct, the
   `neenee-agent` crate, `AgentRequest` / `AgentResponse` / `AgentEvent` /
   `AgentOp` / `AgentNotice` / `AgentIdentity`, `InterAgent*`, and engine
   helpers (`agent_loop`, `agent_setup`, `agent_provider`). `agents` is used
   only as a collective term spanning both roles.
2. **`Principal` — the role layer (top).** The top-level, human-facing agent a
   frontend drives. The user-tunable config table is `[principal]`
   (`PrincipalConfig`); the top-level instance is `principal`.
3. **`Envoy` — the role layer (served).** The isolated child an agent spawns to
   serve a bounded sub-question. Every former `Subagent*` / `subagent` symbol,
   the dispatch tool's name (`"subagent"` → `"envoy"`), its module files
   (`subagent.rs` → `envoy.rs`, `subagent_tool.rs` → `envoy_tool.rs`), and all
   prose become `Envoy` / `envoy`.

The `User-Agent` HTTP header family and the `neenee-agent` crate name are
unaffected: the former is unrelated to the concept, the latter *is* the
umbrella.

This is a hard rename with no compatibility aliases. The `[agent]` config table
is renamed to `[principal]` (an unknown `[agent]` table is now silently
ignored), and the `subagent` tool is renamed to `envoy`.

## Alternatives considered

- **Rename the `Agent` engine struct to `Principal`.** Rejected: the same
  struct runs envoys, so `Principal` would be wrong half the time. The engine
  is the umbrella; only the role is `Principal`.
- **Keep `subagent`, only rename the top-level to `Principal`.** Rejected: the
  collision the change exists to remove lives in the word "subagent".
- **Serde alias `[agent]` → `[principal]` for back-compat.** Rejected:
  pre-1.0, and a clean break keeps the schema unambiguous. Recorded as a
  breaking change in the changelog instead.
- **Rewrite the term inside historical ADRs.** Rejected: accepted ADRs are
  immutable records (see [ADR Workflow](../dev/documentation/adr-workflow.md)).
  Their decision text stays verbatim; only file-path links to moved living docs
  were updated.

## Consequences

- **Breaking (config):** a `[agent]` table no longer applies; users move
  `hard_stop_rounds` / `loop_review_enabled` under `[principal]`.
- **Breaking (tool):** the `subagent` tool is now `envoy`; prompts and any
  external references must use the new name.
- Living docs (reference, explanation, how-to), the glossary, and `CHANGELOG`
  adopt the new vocabulary. Historical ADRs keep their original wording.
- Future multi-agent group chat can speak of several **principals** sharing a
  conversation, each spawning its own **envoys**, without term collision.

## References

- [Sub-agent profiles](0011-subagent-profiles.md) — the envoy profile model.
- [Full-duplex subagent communication](0029-full-duplex-subagent-communication.md) — `EnvoyHandle` / `EnvoyRegistry`.
- [Envoys](../explanation/agent-design/envoys.md), [Glossary](../reference/glossary.md), [Configuration](../reference/configuration.md).
