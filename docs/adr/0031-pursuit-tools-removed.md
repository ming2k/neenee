# 0031. Remove the pursuit tools

- **Status:** Accepted
- **Date:** 2026-06-25

## Context

ADR-0010 slimmed the pursuit primitive to `{objective, is_complete}` and
ADR-0015 introduced the `/pursue` stop-gate plus the `[NEENEE_PURSUIT_COMPLETE]`
marker. Both ADRs kept a layer of three model-facing tools layered onto every
agent — `get_pursuit`, `start_pursuit`, `complete_pursuit` — force-injected by
`Agent::new` from `crates/neenee-store/src/pursuits/tools.rs`. The slimmed
primitive is two fields, yet it carried three tools, a shared
`PursuitToolContext`, a strip-then-reinject step in `Agent::new`, and a block of
system-prompt instructions every turn teaching the model how to use them.

Looking at the responsibilities the tools actually cover, each one is redundant
with an existing mechanism:

- **`get_pursuit` (Read)** reads the pursuit the system prompt already injects
  every turn (`crates/neenee-agent/src/prompt.rs` rebuilds the system message
  each turn and embeds the active `objective`). The model is reading state it
  can already see. The only state that can change mid-turn is completion, and
  that is driven by the model's own actions, not external mutation.
- **`complete_pursuit` (Write)** does exactly what the
  `[NEENEE_PURSUIT_COMPLETE]` marker does. ADR-0015 already names the marker the
  "in-turn path" a running `/pursue` uses and the tool the "interactive turns"
  path — two APIs for one action. The marker is the path the stop-gate already
  consults (`PursuitState::continuation` checks `response.contains(MARKER)`); the
  tool is a duplicate entry point that re-converges on the same persistence
  routine.
- **`start_pursuit` (Write)** lets the model arm a pursuit on its own. Its
  description constrains this to "only when explicitly requested by the user or
  developer instructions" — i.e. the user is present. A present user can type
  `/pursue <condition>`. The tool's real-world use case is near-empty, and the
  constraint is enforced by prompt prose (a soft constraint), not by
  architecture.

The deeper issue is role symmetry. The pursuit lifecycle has three distinct
responsibilities, and ADR-0015's "the gate gates, the model signals" stated only
two of them. The complete picture is:

| Role | Responsibility | Mechanism |
|------|----------------|-----------|
| **User** | Set the condition (entry) | `/pursue <condition>` slash command |
| **Harness** | Drive + gate (continuation) | stop-gate re-injects the condition each round |
| **Model** | Signal completion (exit) | `[NEENEE_PURSUIT_COMPLETE]` marker |

Each role owns one phase; none overlaps. `start_pursuit` breaks the symmetry by
letting the model set its own condition (player and referee). `get_pursuit` and
`complete_pursuit` duplicate mechanisms the entry and exit already own. The
three tools sit between two clean designs (slash command for entry, marker for
exit) and add a third surface that has to be taught, persisted in the tool
table, and burned as tokens in the system prompt every turn.

## Decision

1. **Remove all three pursuit tools.** Delete
   `crates/neenee-store/src/pursuits/tools.rs` (the `GetPursuitTool`,
   `StartPursuitTool`, `CompletePursuitTool`, and `PursuitToolContext` types).
   Remove the `pub mod tools;` export from `crates/neenee-store/src/pursuits/mod.rs`.

2. **Remove the force-inject step in `Agent::new`.** The strip-then-reinject
   block (`tools.retain(|t| !matches!(t.name(), "get_pursuit" | …))` plus the
   three `tools.push(…PursuitTool…)`) is gone. `Agent::new` no longer references
   `pursuits::tools` at all. Externally supplied tools with those names are no
   longer specially handled — if a caller ever passed one, it would just be
   another tool in the list. (No caller does.)

3. **Remove the pursuit-tool guidance from the system prompt.** The block in
   `build_system_message` that instructed the model to "Use get_pursuit to read
   … start_pursuit when … complete_pursuit to mark …" is gone. The system prompt
   still surfaces the active `objective` for visibility; it just no longer
   advertises tools that no longer exist.

4. **The marker is the sole completion path.** `PursuitState::continuation`
   already consults `[NEENEE_PURSUIT_COMPLETE]`; that is unchanged. The
   `complete_pursuit(complete)` tool call path is removed. The orchestration
   finalization that calls `pursuit_service.mark_complete()` after a successful
   turn is unchanged.

5. **`/pursue <condition>` is the sole entry path.** The slash command handler
   (`crates/neenee-cli/src/handlers/slash.rs`) already persists the condition
   via `PursuitService::set_pursuit`, arms the gate, and drives the turn via
   `orchestration::start_pursuit`. That is unchanged. The model has no way to
   start a pursuit on its own — by architecture, not by prompt prose.

6. **Drop the `pursuit` source classification in `snapshot_tools`.** The
   `source` field on `ToolInfo` no longer needs a `"pursuit"` branch because no
   tool carries those names. The classification collapses to `mcp:<server>`,
   `subagent`, or `builtin`.

7. **Update the continuation prompt templates.** The `continuation.md` and
   `objective_updated.md` prompt templates previously told the model to "call
   `complete_pursuit` with status `complete`". They now tell the model to emit
   the `[NEENEE_PURSUIT_COMPLETE]` marker. The completion audit prose is
   preserved; only the signalling instruction changes.

## Alternatives considered

- **Keep `start_pursuit` alone, drop the other two.** Rejected. The role
  symmetry argument applies: the model should not set its own condition. The
  "only when explicitly requested" constraint is a prompt-level guard that
  architecture can enforce more honestly by not offering the tool at all. And
  keeping one tool means keeping the `PursuitToolContext`, the force-inject
  scaffolding, and the system-prompt guidance surface area for a near-empty use
  case.

- **Keep `complete_pursuit` as the sole completion path and drop the marker.**
  Rejected. The marker is what the running stop-gate consults each round; making
  completion a tool call would force the gate to parse tool calls in addition to
  text, and the marker is already stripped from visible output. The marker is
  the lighter-weight, in-band signal; the tool was the heavier duplicate.

- **Rename `start_pursuit` to `pursuit` (collapse to one verbless tool).**
  Rejected. The tool naming convention across the codebase is verb/action
  (`read_file`, `write_file`, `ask_user`, `create_project`); a bare noun
  suggests a CRUD surface over the whole primitive, which would be inaccurate
  for a single-action tool. But this is moot once the tool is removed.

- **Merge pursuit into `/repeat` as a second axis (`/repeat [time]` vs
  `/repeat [condition]`).** Rejected. The two mechanisms share only the
  abstract notion of "driving work beyond one turn". Their structures are
  orthogonal and share no code: `/repeat` is a background cron tick that
  dispatches independent turns; `/pursue` is an in-turn stop-gate that shares
  one message history. Merging them was the exact confusion ADR-0015 resolved
  by splitting the old `/loop`.

## Consequences

Positive:

- The pursuit primitive's interface is now three mechanisms owned by three
  roles: `/pursue` (user), stop-gate (harness), `[NEENEE_PURSUIT_COMPLETE]`
  (model). No redundant surface.
- `Agent::new` no longer strips and reinjects tools; the pursuit state and the
  tool list are decoupled. `PursuitToolContext` is gone.
- The system prompt stops advertising three tools and stops spending tokens on
  usage guidance for them every turn. The active `objective` is still surfaced.
- `crates/neenee-store/src/pursuits/tools.rs` (~250 lines) and its tests are
  deleted. The `pursuits` module shrinks to `service` + `store`.
- `snapshot_tools` loses a dead classification branch.
- The completion path is singular: the marker. No second API to document, test,
  or keep in sync with the marker.

Negative:

- A model can no longer start a pursuit without the user typing `/pursue`. The
  previous design allowed this in principle (gated by a soft prompt
  constraint); this design forbids it by architecture. Any workflow that relied
  on a developer instruction telling the model to "start a pursuit" must now
  have the user run `/pursue` explicitly.
- `complete_pursuit` is no longer available to interactive (non-`/pursue`) turns
  that wanted to mark a pursuit complete through the permission broker. The
  marker path does not go through the broker (it is a text signal, not a tool
  call). In practice the marker is what running pursuits already used; the
  brokered tool path was unused for interactive completion. The `/pursue done`
  slash command remains the user-driven completion path.

Neutral:

- The `[NEENEE_PURSUIT_COMPLETE]` marker, `PursuitState`, `PursuitService`,
  `PursuitStore`, the stop-gate in `execute_turn`, and the
  `orchestration::start_pursuit` driver are all unchanged. The persistence
  schema is unchanged. This ADR only removes the tool layer; the primitive, the
  driver, and the signal stay.

## References

- [ADR-0010](0010-slim-goal-primitive.md) — slimmed the pursuit primitive;
  point 4 kept the model-facing tools. This ADR reverses that sub-decision.
- [ADR-0015](0015-pursue-stop-gate-and-repeat-cron.md) — introduced the
  stop-gate and the marker; kept the tool layer. This ADR reverses the
  tool-keeping sub-decision and keeps the stop-gate + marker.
- `crates/neenee-store/src/pursuits/tools.rs` — deleted.
- `crates/neenee-agent/src/agent.rs` — `Agent::new` and `snapshot_tools`
  simplified.
- `crates/neenee-agent/src/prompt.rs` — `build_system_message` drops the
  tool-usage guidance.
- `crates/neenee-core/src/pursuits/prompts/continuation.md` and
  `objective_updated.md` — completion instruction rewritten to use the marker.
