# 0025. Lifecycle event hooks: user-configurable interception at session/turn/tool points

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

neenee cannot run a user-defined action at a lifecycle point without a code
change to `agent.rs` / `orchestration.rs`. Concrete wants this blocks today:
format-on-write, a CI gate before a turn ends, a session-start notification,
injecting "keep the test files" into compaction. Each currently earns a bespoke
code path.

The codebase has **four prior "hook" abstractions**, but investigating them
shows they are not a foundation to build on — they are one-shot traits that
over-abstracted a single need:

| Abstraction | Location | Implementations | Verdict |
|---|---|---|---|
| `HooksConfig` (`post_plan`/`pre_complete`) | `config.rs:212-217` | `post_plan` has **zero** consumers (dead); `pre_complete` used once at `plan_verify.rs:129` | Dead-end schema: hard-codes two phase names, runs only shell strings |
| `CompactionHooks` trait | `session.rs:1561-1568` | `RelayCompactionHooks` (just emits an activity string to the TUI), `NoopCompactionHooks`, one test `VetoHooks` | The production impl does nothing a hook user cares about |
| `ContextReliefGate` trait | `capability.rs:74-77` | `MidTurnPruneGate` only | A mid-turn pruning *strategy*, not an interception point |
| `SessionReview` trait list | `session_review.rs:109` | `LoopingReview` only | A *prompt fragment* registry for the diagnostic sub-agent — a different species |

None of these lets a user say "when the agent finishes editing, run my linter".
They each wrap one internal concern and were parameterised into traits before a
second implementation ever appeared.

The reference point — Claude Code — was checked two ways. Its `2.1.187` binary
was reverse-engineered (event taxonomy extracted), and its published hooks
reference was read in full. Both agree on a deliberately **narrow** model:

- **One axis: lifecycle events**, grouped by cadence — *per session*
  (`SessionStart`/`SessionEnd`), *per turn* (`UserPromptSubmit`/`Stop`/
  `StopFailure`), *per tool call* (`PreToolUse`/`PostToolUse`/`PostToolUseFailure`).
- **Capability is implicit in the event**, not chosen by the user:
  `PreToolUse` may deny, `Stop`/`PostToolUse`/`UserPromptSubmit` may inject
  context, the rest only observe. The user never names a trait or decision type.
- **Context threshold, round count, and clock are not hook axes.** They are
  built-in deterministic behaviour (compaction fires at a configured utilisation;
  there is no "NearCompact at 70%" hook). Its one time-shaped event,
  `FileChanged`/`CwdChanged`, is a concrete file-watch event, not a generic
  "Time axis".

neenee already has the deterministic engines Claude Code keeps internal:
`CompactionPolicy`/`ContextBudget` (threshold), `/pursue` stop-gate (ADR-0015),
`/repeat` cron (ADR-0015). Exposing those again as hook *axes* would duplicate
them and muddy two clean concepts. What neenee lacks is exactly what Claude
Code's hooks provide: **user-configurable actions on lifecycle events.**

## Decision

Add a single lifecycle-event hook system. Do **not** build a multi-axis bus and
do **not** adapt the one-shot traits onto it.

### One axis: lifecycle events

```rust
// neenee-core/src/hooks.rs — pure domain, no IO
pub enum HookEvent {
    SessionStart { source: SessionSource },   // startup | resume
    SessionEnd,
    UserPromptSubmit { prompt: String },
    PreToolUse  { tool_name: String, tool_input: serde_json::Value },
    PostToolUse { tool_name: String, tool_output: ToolOutput, duration_ms: u64 },
    PostToolUseFailure { tool_name: String, error: String },
    Stop { last_message: String },
    PreCompact,
    PostCompact,
}
```

No `NearCompact`/`Every`/`Interval` events. The threshold is `CompactionPolicy`
(unchanged); periodic work is `/repeat`. If a genuine "every N turns" need
appears later, it composes as a `Stop` hook with internal counting — not a new
axis.

### One trait, one output enum

```rust
pub enum HookOutcome {
    /// No effect; continue normally.
    Pass,
    /// Block the action (PreToolUse) or stop the turn (Stop), feeding
    /// `reason` back to the model.
    Deny { reason: String },
    /// Inject `context` as a hidden user message (UserPromptSubmit / Stop /
    /// PostToolUse). The turn continues.
    Inject { context: String },
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn event(&self) -> HookEventKind;        // which events this hook wants
    fn matcher(&self) -> Option<&str>;        // tool-name filter, None = all
    async fn fire(&self, ctx: &HookContext) -> HookOutcome;
}
```

A single trait returning one enum is chosen deliberately. The four-capability
split (Observer/Guard/Transform/Continuation) was drafted and rejected (see
Alternatives): it quadruples dispatch boilerplate to buy type-safety the user
never sees, because **users write TOML, not Rust**. The trait shape is internal
plumbing; one enum keeps that plumbing minimal. How an outcome is *honoured*
depends on the insertion point (a `Deny` from `PostToolUse` is nonsensical and
ignored; from `PreToolUse` it blocks), which is documented per event, not typed.

### Handler type: command only (for now)

```toml
# neenee config.toml — reuses the existing [hooks] table, generalised
[[hooks.on]]
event   = "PostToolUse"
matcher = "Write|Edit"                 # tool-name: exact | pipe-list | regex
command = ".neenee/hooks/lint.sh"      # stdin = HookContext JSON; cwd = project root

[[hooks.on]]
event   = "PreToolUse"
matcher = "Bash"
command = ".neenee/hooks/guard-rm.sh"

[[hooks.on]]
event   = "Stop"
command = ".neenee/hooks/ci-gate.sh"
```

The runner spawns the command, writes the `HookContext` as JSON on stdin, and
parses stdout: `exit 2` + stderr is a `Deny`; a JSON object on stdout
(`{"decision":"deny"|"approve", "reason":"…", "context":"…"}`) is honoured per
event; `exit 0` with no output is `Pass`. This is the Claude Code contract,
flattened to suit TOML/Rust rather than JS nested objects.

`http`/`mcp_tool`/`prompt` handler types are **out of scope** for this ADR.
`command` covers the 90% case (lint, format, CI, notify, guard scripts). They
can be added later behind the same `Hook` trait without touching insertion
points.

### Matcher — same rules as Claude Code

`"Write|Edit"` = pipe-separated exact names; any other character makes it a
regex; omitted/`"*"` matches all. Only tool events honour it; the rest ignore
it. MCP tools surface as `mcp__<server>__<tool>` and match identically.

### Layering

- `neenee-core/src/hooks.rs` — `HookEvent`, `HookOutcome`, `Hook` trait,
  `HookContext`. No IO.
- `neenee-agent/src/hooks/` — `HookRegistry` (a `Vec<Arc<dyn Hook>>` filtered by
  event kind, mirroring the existing `reviews` list at `agent.rs:108`), and the
  insertion calls.
- `neenee-store/src/config.rs` — replace `HooksConfig{post_plan,pre_complete}`
  with the `[[hooks.on]]` table.
- `neenee-cli` — the command runner (spawn, stdin JSON, stdout/exit parse).

### Insertion points (single funnel each, all pre-identified)

| Event | Funnel | Outcome honoured |
|---|---|---|
| `PreToolUse` | `execute_tool` top, `agent.rs:1789` | `Deny` blocks; `Inject` ignored |
| `PostToolUse`/`PostToolUseFailure` | `record_tool_result`, `agent.rs:1383` | `Inject` |
| `Stop` | beside `pursuit_continuation`, `agent.rs:1227`/`:987` | `Deny`→continue w/ reason; composes with `/pursue` (both must agree to stop) |
| `UserPromptSubmit` | `execute_turn` entry, `orchestration.rs:373` | `Deny` drops prompt; `Inject` prepends context |
| `PreCompact`/`PostCompact` | `run_compaction`, `session.rs:1591`/`:1642` | `Inject` folds into summary context |
| `SessionStart`/`SessionEnd` | driver (`neenee-cli`), wrapping `SessionStore::{load_for_project,resume,reset}` | observe only |

### What happens to the one-shot traits

- **`CompactionHooks`** — **deleted.** Its sole real behaviour (emitting an
  activity string) moves inline into `run_compaction`. The `pre_compact`/
  `post_compact` *user* surface becomes `PreCompact`/`PostCompact` events on the
  new bus. `CompactionDecision`'s veto is dropped — there is no evidence anyone
  vetoes compaction, and a hook that prevents relief while pressure keeps rising
  is a footgun.
- **`ContextReliefGate`** — **untouched.** It is a mid-turn pruning strategy
  (`MidTurnPruneGate`), not an interception point. Folding it in would conflate
  "what the harness does under pressure" with "what the user adds on events".
- **`SessionReview`** — **untouched.** Prompt fragments for the diagnostic
  sub-agent; a different concern.
- **`HooksConfig`** — the `[[hooks.on]]` table replaces `post_plan`/
  `pre_complete`. `pre_complete`'s single use (plan-verify test commands) is
  kept by mapping it internally to a `PostToolUse`-equivalent on the verify
  tool, or by leaving `plan_verify.rs:129` as a direct config read until the
  verify tool emits the right event. `post_plan` (dead) is just removed.

## Alternatives considered

- **A four trigger-axis bus (Event / Context / Round / Time) × four capability
  traits (Observer / Guard / Transform / Continuation).** This was the first
  draft. Rejected on three grounds: (1) it duplicates neenee's existing
  threshold/round/clock engines, which Claude Code deliberately keeps internal
  too; (2) it canonises the one-shot traits by "adapting" them instead of
  removing them; (3) four traits buy type-safety invisible to users who only
  write TOML, while quadrupling dispatch code. A single event axis matching
  Claude Code is simpler, proven, and enough for every concrete want listed in
  Context.
- **Adapt the existing traits onto a bus instead of deleting them.** Rejected:
  every one has a single real implementation, so the trait is pure overhead
  (YAGNI). `CompactionHooks::RelayCompactionHooks` forwards one string;
  inlining it is less code than an adapter. Keeping dead abstractions to avoid
  churn guarantees they accrete.
- **God-trait with `Box<dyn Any>` payload.** Rejected: `HookContext` is a typed
  struct carrying the event + session metadata; the only varying return is the
  small `HookOutcome` enum. No need for type-erased payloads.
- **Add return values to the `AgentEvent` callback.** Rejected (as in the first
  draft): `AgentEvent` is a one-shot `FnMut` relay fanned out to the TUI;
  turning it into a control channel couples render latency to loop control and
  breaks its single-receiver assumption. Signal and control stay separate.
- **Port Claude Code's JSON schema verbatim, including `http`/`mcp_tool`/
  `prompt`/`agent` handler types.** Rejected for v1: scope creep. `command`
  covers the documented wants; the `Hook` trait is shaped so the other handler
  types slot in later without re-touching insertion points.

## Consequences

- **Positive:** users configure format-on-write, CI-on-stop, session-start
  notify, compaction-context injection, and Bash guards **without any core
  code change** — a `[[hooks.on]]` entry each. One small trait + one enum +
  one registry replace four divergent one-shot abstractions; net code likely
  shrinks. The model matches Claude Code closely enough that prior art and
  user intuition transfer.
- **Negative:** breaking change to the `[hooks]` table (`post_plan`/
  `pre_complete` removed). Usage appears near-zero (`post_plan` dead,
  `pre_complete` one internal call site), so migration is local. `CompactionHooks`
  removal touches `run_compaction` and its call sites. Adding insertion calls in
  `agent.rs`/`orchestration.rs` is unavoidable but each is a single funnel.
- **Neutral:** `ContextReliefGate` and `SessionReview` stay exactly as they are;
  they are not "part of hooks" and this ADR makes no claim on them. `/pursue`
  and `/repeat` remain the stop-gate and clock engines; `Stop` hooks compose
  with `/pursue` (stop requires both to agree).
- **Migration (staged, each step shippable):**
  1. `neenee-core/src/hooks.rs` — types + trait; `HookRegistry`; insertion calls
     at `PreToolUse`/`PostToolUse`/`Stop` with the registry empty by default.
  2. `[[hooks.on]]` config + command runner in `neenee-cli`; ship `command`
     handlers for those three events.
  3. Add `UserPromptSubmit`, `PreCompact`/`PostCompact`, `SessionStart`/`End`.
  4. Delete `CompactionHooks` (inline its activity emit); replace `HooksConfig`;
     remap `pre_complete`.
  5. (Optional, later) `http`/`mcp_tool` handler types behind the same trait.

## References

- [ADR-0009](0009-uncapped-agentic-loop.md) — uncapped loop; `Stop` hooks must
  not reintroduce a blanket round cap.
- [ADR-0015](0015-pursue-stop-gate-and-repeat-cron.md) — `/pursue` stop-gate and
  `/repeat` cron already own the stop-gate and clock concerns; `Stop` hooks
  compose with `/pursue`, they do not replace it.
- [ADR-0019](0019-model-relative-context-compaction.md) — compaction thresholds
  stay deterministic in `CompactionPolicy`; only `PreCompact`/`PostCompact`
  *events* are hookable.
- [ADR-0021](0021-pruning-is-implicit-and-distinct-from-compaction.md) —
  `ContextRelief*` stays implicit and distinct; not folded into hooks.
- Claude Code `2.1.187` binary (reverse-engineered) + published hooks reference —
  single event axis, implicit per-event capability, command-handler-first; the
  model this ADR adopts, minus the handler types deferred to a later ADR.
