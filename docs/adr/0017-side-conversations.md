# 0017. Side conversations: session-native `/btw`

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

A `/btw` command lets the user open a side conversation — a quick, separate
chat — while the main session keeps its focus. The reference UX is codex's
`/side` / `/btw` (alias), described in `codex-rs/tui/src/app/side.rs`: an
ephemeral fork of the current thread that (a) inherits the parent history as
reference-only context, (b) keeps running independently, and (c) surfaces the
parent's status ("main needs approval") in a banner while the user is inside
it.

neenee already has every primitive a side conversation needs in isolation, but
no way to compose them concurrently:

- **Fork primitive.** `SessionStore::fork`
  (`crates/neenee-store/src/session.rs:660`) mints a child id and records
  `parent_id` lineage. But it **flips the active pointer** to the child
  (`data.id = …` at line 673): after `fork()`, the same store serves the child,
  not the parent.
- **Single active session.** `SessionStore` holds one `data: Mutex<SessionData>`
  (`session.rs:384`). `messages()`, `replace_messages()`, compaction, and
  `execute_turn`'s mid-turn persistence all operate on whichever session is
  currently loaded into `data`. There is no way to address two sessions through
  one store; this is a **semantic** conflict over the active pointer, not a
  mutex race — two turns sharing one store would clobber each other's history.
- **Single live agent + turn.** `main.rs` holds one `agent: Arc<Agent>`, one
  `history`, one `current_task_token` + `task_generation`
  (`main.rs:290-291`). `start_interactive_turn` allows exactly one in-flight
  turn (`orchestration.rs:269`). Starting a side conversation the obvious way
  (switch active session like `/session open` does at `main.rs:955`) **cancels
  the parent's turn** — the opposite of the codex experience.
- **Events have no session id.** Every `AgentResponse` variant
  (`crates/neenee-core/src/events.rs:73`) is implicitly "for the active
  session". The TUI renders a single `messages: Vec<TranscriptMessage>`. There
  is no field to route an event to a side transcript.
- **Subagents are the wrong shape.** `TaskTool`
  (`crates/neenee-agent/src/task_tool.rs`) *can* spawn a fresh `Agent`, but it
  is autonomous and one-shot: per ADR-0011, `forward_event` **drops
  `UserQuestionRequest`** (`task_tool.rs` defensive arm) because a sub-agent has
  no reachable user. Its transcript attaches to one turn via
  `Message::children` (`message.rs:62`), so it cannot be resumed independently
  later. A multi-turn, interactive, recoverable side conversation is not a
  `TaskTool` subagent.

So the question is not "can we build `/btw`" — the pieces exist — but "what is
the minimal structural change that lets two sessions live at once, with events
routed to the right transcript, while the parent turn keeps running."

## Decision

Make **session identity first-class on the protocol**, and let the harness hold
a **primary session plus at most one live side session**, each with its own
agent and its own self-contained session file.

1. **Tag session-scoped events with a session id.** Split the per-turn shapes
   off `AgentResponse` into a `TurnEvent` enum and carry them under an
   envelope:

   ```rust
   pub enum AgentResponse {
       Turn { session_id: String, event: TurnEvent },
       // …global variants unchanged: ProviderPicker, SessionsOverview,
       //   SessionContext, ProviderKeys, Exit, ProviderSwitched, …
   }
   ```

   `TurnEvent` holds the shapes that belong to a specific turn/stream:
   `Text`, `Stream*`, `ToolCall`, `ToolResult`, `ToolStream`,
   `ToolCancelled`, `RoundStarted`, `Activity`, `PermissionRequest`,
   `UserQuestionRequest`, `Compacted`, `RetryScheduled`, `SessionReview`,
   `PursuitUpdated`, `ModeChanged`, `PlanProgressUpdated`, `UnattendedChanged`,
   `HarnessState`, `SubTask`. `relay_agent_event`
   (`orchestration.rs:547`) emits `Turn { session_id, event }` for the turn it
   is relaying; global events stay top-level. The TUI keys transcript buffers
   by `session_id` and routes each `Turn` to the right one.

2. **A `SessionRegistry` owns the live set: `{ primary, side: Option<_> }`.**
   Each entry is a `LiveSession { id, agent: Arc<Agent>, store:
   Arc<SessionStore>, history, token_slot, generation }`. Two live sessions get
   two `Agent` instances — already a proven pattern (`TaskTool::new` constructs
   a fresh `Agent` per sub-agent, `task_tool.rs:182`). They share the provider
   through the existing `ProxyProvider` (`orchestration.rs:40`), which clones
   the inner `Arc<dyn Provider>` per call and is therefore safe under
   concurrency. The cap is one side at a time, matching codex's "A side
   conversation is already open" rule; lifting the cap later is the only step
   needed to generalize.

3. **Add `SessionStore::fork_to_side() -> (side_id, parent_id)`**
   (`session.rs`, peer of `fork`). Unlike `fork()`, it **writes the current
   snapshot to a self-contained side file** (`<project>/sessions/<side_id>.json`
   plus its own `events.jsonl`) and returns the id **without changing the
   primary's active pointer**. The side `LiveSession` is then constructed with
   `SessionStore::for_path(<side file>)` (`session.rs:479`), so the side's
   persistence never touches the primary's `session.json` / `events.jsonl`. The
   two stores coexist because they write different files in the same project
   bucket. This is the fix for the single-active-pointer conflict: instead of
   refactoring `SessionStore` to operate-by-id, give each live session its own
   store pinned to its own file.

4. **`/btw [prompt]` enters a zoom-in side view** — a full-screen transcript
   swap, not a modal. The visual language is reused from the existing sub-agent
   zoom (`App::focus_stack` at `tui/app.rs:188`, `draw_subagent_bar` at
   `tui/render/step/renderers.rs:1111`): a top banner reads
   `Side from main · <parent status> · Esc back`. The composer, message stream,
   and tool-step rendering are unchanged; only the data source (which buffer
   the view reads) and the banner differ. `Esc` / `Ctrl+C` returns to the
   primary view. The side session stays on disk and reappears in `/sessions`
   (recoverable), distinguished by its `parent_id`.

5. **Parent status passthrough.** While the side view is active, the registry
   watches the primary `LiveSession` and emits a new global
   `AgentResponse::ParentStatus(ParentStatus)` carrying
   `Idle | Running | NeedsApproval | NeedsInput | Failed | Interrupted`. The
   banner renders it verbatim. This is the codex `SideParentStatus`
   (`codex-rs/tui/src/app/side.rs:54`) equivalent and is the whole reason the
   parent turn is left running instead of cancelled: the user can see the main
   session hit an approval or input wall and jump back.

The turn machinery itself (`execute_turn`, `start_interactive_turn`) is reused
unchanged — it already takes a `TurnContext` / `InteractiveTurnContext`
bundling `agent` + `history` + `session` + `token_slot`
(`orchestration.rs:232-267`); `/btw` simply builds a second context bound to
the side `LiveSession` and calls the same functions.

## Alternatives considered

- **One-shot subagent via `TaskTool` + `Message::children`** (the literal
  reading of "via subagents"). Rejected: subagents are non-interactive by
  design (ADR-0011 drops `UserQuestionRequest`), so they cannot host a
  multi-turn chat; and `children` attach to a single turn, so a side
  conversation could not be resumed independently via `/sessions`. Meets neither
  the "multi-turn" nor the "recoverable fork" requirement.

- **Parked parent (switch active session on `/btw`, like `/session open`
  today).** Rejected: it cancels the parent turn (`main.rs:955` cancels the
  token), which discards an in-flight pursuit's progress and makes
  parent-status passthrough meaningless (there is no running parent to report
  on). This is exactly the compromise the design brief rejects.

- **General N-session manager from day one.** Rejected as over-scoped: codex
  itself caps at one side at a time, and every realistic `/btw` use is one
  aside. The `{ primary, side: Option<_> }` shape generalizes to a `HashMap`
  later by lifting the cap and the "one side" UI assumption — no structural
  rewrite required.

- **Refactor `SessionStore` to operate-by-id on every method** (pass `id` to
  `messages` / `replace_messages` / compaction). Rejected for now: large API
  churn across the turn hot path. The self-contained-side-file approach
  (`fork_to_side` + `for_path`) reaches the same concurrency with an additive
  change and reuses `SessionStore` wholesale; the active-pointer semantics stay
  intuitive (one store = one session = one file). Operate-by-id remains
  available under a later ADR if a second concurrent session becomes common.

- **Tag only side events with a `Side { event }` variant, leave primary events
  untagged.** Rejected: asymmetry between primary and side is the kind of
  special-casing that ages poorly. The uniform `Turn { session_id, event }`
  envelope makes "which session does this belong to" a first-class question,
  which is the actual generalization codex's thread-id-on-everything model
  buys.

## Consequences

Positive:

- **Two sessions live at once, by construction.** The parent turn keeps running
  while the user chats in the side; parent-status passthrough is real, not
  cosmetic. This is the codex experience the brief targets.
- **Session becomes a first-class protocol concept**, not an implicit
  singleton. Future work (parallel background tasks, a richer `/session`
  switcher, per-session pursuit state) composes naturally — the envelope is the
  foundation, not a one-off.
- **Reuses proven primitives.** `fork_to_side` is a peer of `fork`; the side
  `Agent` is constructed exactly like `TaskTool`'s sub-agent; the zoom-in view
  reuses `focus_stack` / `draw_subagent_bar`; `/sessions` lists the side for
  free because it is just another `sessions/<id>.json` with a `parent_id`.
- **Isolated persistence.** The side writes only its own file; the primary's
  `session.json` and event log are untouched while a side is open, so a crash
  mid-side never corrupts the main session.

Negative:

- **Event protocol gains an envelope.** Every per-turn emitter wraps once in
  `relay_agent_event`, and the TUI's single `match` on `AgentResponse` gains a
  `Turn { session_id, event } => route(session_id, event)` arm that dispatches
  a `TurnEvent` sub-match into the right buffer. Mechanical but touches the
  render hot path; mitigated by keeping global variants top-level so
  non-session-scoped handling is unchanged.
- **Two live `Agent` instances share mutable singletons indirectly.** The
  per-project permission allowlist and skills registry are shared. The
  allowlist is append-only and already synchronized; skills are read-only after
  load. Concurrent turns therefore do not introduce new shared-mutability
  hazards beyond what `TaskTool` already exercises.
- **`SessionStore` gains a second fork entry point.** `fork` (flip-active) and
  `fork_to_side` (write-side-file) coexist. The distinction is documented on
  each method; `fork`'s existing callers (`/session fork`) are unchanged.

Migration:

- None for existing sessions or commands. `/btw` is additive; `fork` and
  `/session fork` keep their current semantics. The `AgentResponse` envelope is
  an internal protocol change (the binary and TUI ship together), so no
  on-disk format migration is involved.

## Implementation

Landed alongside this ADR being marked `Accepted`. The five decision points
map to the code as follows:

1. **`Turn { session_id, event }` envelope** — already in place; the TUI's
   response listener now routes each `TurnEvent` to the side buffer when its
   `session_id` matches the live side session and to the primary buffer
   otherwise (`crates/neenee-cli/src/tui/mod.rs`).
2. **Live-session ownership** — instead of refactoring the primary's loose
   per-turn state into a `SessionRegistry`, the primary machinery is left
   exactly as-is and peered with an optional `SideSession`
   (`crates/neenee-cli/src/main.rs`) bundling its own `Agent`, `SessionStore`,
   history mutex, token slot, and generation counter. An `active_view_side`
   flag routes `Chat` to whichever session the user is composing into via
   `start_active_turn`. This is the minimal `{ primary, side: Option<_> }`
   shape; generalizing to a `HashMap` later is the only step needed for N>1.
3. **`fork_to_side` + `open_side`** — unchanged; the `/btw` arm calls them to
   mint the self-contained side file and load a peer store pinned to it.
4. **`/btw [prompt]` zoom-in view** — a top banner (`draw_side_banner`,
   `Side from main · <status> · Esc back`) carved off the transcript viewport;
   `Esc` and `Ctrl+C` exit (send `AgentRequest::ExitSideView`). The composer
   stays live in the side view, unlike the sub-agent zoom.
5. **Parent-status passthrough** — `spawn_parent_status_watcher` polls the
   primary token slot and emits `AgentResponse::ParentStatus` on change.

Four scoping choices, all deferring polish without contradicting the decision:

- **Side `Agent` runs with `unattended(true)`** (mirroring `TaskTool`'s
  sub-agent). A side chat is a quick aside; suppressing its permission prompts
  sidesteps the fact that the shared permission-reply channel routes to the
  primary `Agent`. The primary's prompts are unaffected and still modal. A
  side `UserQuestionRequest` is forwarded to whichever agent owns it
  (`reply_user_question` tries primary then side).
- **The side toolset excludes `TaskTool`** (mirrors the sub-agent profile
  filter), so a side chat cannot spawn nested sub-agents.
- **Global responding/activity/harness state is gated to the primary session.**
  A concurrent side turn drives only its own transcript buffer; the primary
  view's chrome is never disturbed. The side view's "is it working" signal is
  the transcript growing plus the parent-status banner.
- **`ParentStatus` currently surfaces only `Running`/`Idle`** (derived from
  the primary token slot). The remaining variants are carried by the enum but
  not yet populated by the watcher — a primary permission/question still
  surfaces globally as a modal, which covers the same "main hit a wall" UX.
  Wiring the finer-grained statuses is additive future work.

## References

- `crates/neenee-core/src/events.rs` — `AgentResponse`, `TurnEvent` envelope,
  `ParentStatus`.
- `crates/neenee-store/src/session.rs` — `fork_to_side` (peer of `fork` at
  line 660); `for_path` (line 479) for the side store.
- `crates/neenee-agent/src/orchestration.rs` — `relay_agent_event` emits the
  `Turn` envelope; `InteractiveTurnContext` (line 255) reused for side turns.
- `crates/neenee-cli/src/main.rs` — `/btw` dispatch arm; `SessionRegistry`.
- `crates/neenee-cli/src/tui/app.rs` — per-session transcript buffers, view
  selector; `focus_stack` (line 188) visual reuse.
- Predecessor: [ADR-0011](0011-subagent-profiles.md) — why a side conversation
  is not a `TaskTool` subagent (non-interactive, turn-bound transcript).
- Predecessor: [ADR-0014](0014-xdg-persistence-architecture.md) — the project
  session bucket layout the self-contained side file lives in.
- Reference implementation: `codex-rs/tui/src/app/side.rs` (`SideParentStatus`,
  the side-fork lifecycle).
