# ADR-0047: Round contains turn (vocabulary swap)

Date: 2026-07-XX

## Status

Accepted

## Context

Since the project's inception the two execution layers were named the way
most LLM-agent tooling names them: a **turn** was the unit the user
perceives (one submitted message → one final reply), and a **round** was
one iteration of the ReAct loop inside it. The canon
([`turns-and-rounds.md`](../explanation/agent-design/rounds-and-turns.md)),
the glossary, every control-plane symbol (`execute_turn`, `TurnEvent`,
`AgentResponse::Turn`), the activity-modal detail line (`turn N · round M`),
and the user-facing config keys (`hard_stop_rounds`, the `Round` hook event)
all encoded that convention.

The team came to find the inverse mapping more intuitive in practice:

- A **round** reads naturally as one complete exchange — analogous to a
  round in a game, a voting round, or a round of a fight — one full
  back-and-forth between the user and the agent.
- A **turn** then reads as one *turn to move* inside that round — the
  model takes its turn, a tool takes its turn.

The existing mapping forced the larger, user-perceived unit onto the smaller
word and the loop iteration onto the larger word, which is the opposite of
how "round" and "turn" are used in ordinary English. The concepts and their
semantics were never in question; only the labels were felt to be backwards.

## Decision

Swap the two words throughout the codebase, canon, and UI, so that:

- **round** = the user-perceived unit (one submitted message → one final
  reply). The round counter persists across rounds.
- **turn** = one iteration of the ReAct loop inside a round. The turn
  counter resets each round.

This is a pure relabelling. No behavior, control flow, persistence format,
or wire protocol changes — only which word names which concept.

### Renamed symbols (non-exhaustive)

| Old | New |
|-----|-----|
| `TurnEvent` | `RoundEvent` |
| `AgentResponse::Turn` | `AgentResponse::Round` |
| `TurnOutcome` / `TurnTimer` | `RoundOutcome` / `RoundTimer` |
| `TurnInput` / `TurnContext` / `InteractiveTurnContext` | `RoundInput` / `RoundContext` / `InteractiveRoundContext` |
| `execute_turn` / `start_interactive_turn` | `execute_round` / `start_interactive_round` |
| `compact_turn_history` | `compact_round_history` |
| `RoundStarted` | `TurnStarted` |
| `append_round` | `append_turn` |
| `set_round_persist` | `set_turn_persist` |
| `hard_stop_rounds` (config key) | `hard_stop_turns` |
| `review_start_round` / `review_interval_rounds` | `review_start_turn` / `review_interval_turns` |
| `HookEventKind::Round` / `HookEvent::Round` | `HookEventKind::Turn` / `HookEvent::Turn` (config string `"Round"` → `"Turn"`) |
| `Strategy::RoundBand` / `"round_band"` | `Strategy::TurnBand` / `"turn_band"` |
| `TranscriptMessage::round` / `with_round` | `TranscriptMessage::turn` / `with_turn` |
| `turn_count` / `current_round` (TUI state) | `round_count` / `current_turn` |

### UI

The Activity-modal detail line flips from `turn N · round M · <model>` to
`round N · turn M · <model>`.

### Docs

[`turns-and-rounds.md`](../explanation/agent-design/rounds-and-turns.md) is
renamed to `rounds-and-turns.md` and rewritten in the new vocabulary; the
glossary, status-bar reference, configuration reference, and the
hooks/pursuits/envoys explanations are updated to match.

## Consequences

- **Breaking config change.** `hard_stop_rounds`, `review_start_round`,
  `review_interval_rounds`, and the `event = "Round"` hook value are now
  `hard_stop_turns`, `review_start_turn`, `review_interval_turns`, and
  `event = "Turn"`. Existing user config files using the old keys silently
  fall back to defaults (serde ignores unknown keys). Users migrating must
  rename these keys.
- **Breaking API change** for anything depending on the crate type/function
  names listed above. neenee's crates are not yet at a stable 1.0, so this
  rides a minor bump.
- Historical records — earlier ADRs (notably ADR-0009, ADR-0016, ADR-0030,
  ADR-0034) and `CHANGELOG.md` entries — are left in their original
  vocabulary. They describe decisions as they were made; back-editing them
  would falsify the record. The glossary's *Legacy terms* section could
  eventually gain an entry pointing old names to new ones.
- Internal consistency is restored: the word attached to the larger concept
  is the larger word.
