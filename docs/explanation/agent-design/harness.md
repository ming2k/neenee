# Harness Architecture

The harness is the control plane around provider calls. It keeps model output
inside explicit state, execution, and safety boundaries.

## Turn execution

Every CLI turn runs the streaming agent loop:

1. Refresh the system context with pursuit, tools, and skill metadata.
2. Stream provider text and reconstruct native tool-call deltas by index.
3. Execute native or JSON fallback tool calls through the same registry.
4. Emit tool call/result events for the TUI.
5. Stop on a final assistant message or a harness safety bound.

Streaming remains inside the harness. Text fallback JSON is withdrawn from the
visible transcript before its tool step is emitted.

Provider adapters must preserve the harness system context. OpenAI-compatible
providers use system messages; Gemini maps them to `systemInstruction` and
returns fallback tool results as user-context text.

The TUI merges each tool call and result into a semantic step. Steps are
collapsed to a one-line status by default and `Ctrl+T` toggles complete JSON
arguments and output. Session replay rebuilds the same steps in FIFO order,
including parallel calls with identical tool names.

## Provider capabilities

The harness distinguishes two model capability surfaces. Tools are declared
to the provider on every request; reasoning is observed from the provider
when the model emits it. For the capability model and wire-level protocol,
see [Provider capabilities](../provider-capabilities.md) and
[Tool rounds](turns-and-rounds.md).

### Declared: tools

Tool schemas live inside the provider, not the conversation. Each turn caches
every tool's OpenAI function schema before any network work. Every HTTP
request then re-injects the full cached set as the `tools` field with
`tool_choice: "auto"`.

Tool schemas are request-scoped. Every ReAct round, including the round that
carries tool results back upstream, sends the same complete schema set
alongside the full message history. The provider is stateless across turns.

The OpenAI-compatible providers declare schemas natively: the registry
presets (`kimi-code`, `deepseek-v4-flash`, `deepseek-v4-pro`, `zai-code`) and
the bespoke `openai` entry all share one adapter, so they inherit native tool
declaration. The Gemini and Llama adapters do not override the default and
never send a `tools` field; tool calls on those providers travel only through
the universal fallback below.

### Observed: reasoning

The harness never declares reasoning support and never sends a flag that
requests it. Providers passively read `reasoning_content` from stream deltas
and complete messages, forwarding it as a reasoning event. Only models that
emit the field (`deepseek-v4-flash` thinking mode, reasoning-tuned GLM and
Qwen variants) surface reasoning; other models produce none.

Reasoning is rendering metadata. It is not summarized, not re-injected as
follow-up context, and not used for control flow.

### Tool call transport

Both execution paths feed one shared registry:

| Path | Transport | Tool calls |
|------|-----------|-----------|
| Non-streaming | Single HTTP round trip | `choices[0].message.tool_calls` complete |
| Streaming | SSE stream | `delta.tool_calls` fragments accumulated by `index` |

The streaming path accumulates `id`, `name`, and `arguments` per index while
text and reasoning deltas render live. After the stream reaches `[DONE]`,
calls with an empty `id` are assigned `call_<uuid>`, calls with an empty
`name` are dropped, and the survivors are executed. Side effects never fire
mid-stream.

### Universal fallback

For providers without native function calling, the harness extracts
`{"tool": "<name>", "arguments": {…}}` from assistant text and promotes the
parsed call onto the preceding assistant message as a native `tool_calls`
entry so OpenAI-compatible `tool_call_id` pairing stays valid on the next
round.

Fallback text is withdrawn from the visible transcript before the tool step
is emitted, matching the native streaming path. The same registry, permission
broker, and result-message format apply to native and fallback calls.

## Pursuit state

`/pursue <condition>` creates a durable, per-session objective persisted as a
field on `SessionData` (`Option<Pursuit>` via `SessionStore`, ADR-0032), so it
survives restarts and `/resume`. A pursuit is a slim primitive: an objective
and a single `is_complete` flag (no status machine, no token or time budget,
no checklist — all removed; see
[ADR-0010](../../adr/0010-slim-goal-primitive.md) and
[ADR-0015](../../adr/0015-pursue-stop-gate-and-repeat-cron.md)). There are no
model-facing pursuit tools: the user sets the condition via `/pursue`, the
harness drives continuation via the stop-gate, and the model signals completion
with `[NEENEE_PURSUIT_COMPLETE]` (ADR-0031). See
[Pursuits](pursuits.md) for the primitive and the persistence model.

## Pursue stop-gate

`/pursue` arms a **stop-gate** on the agent and drives one turn. Each time the
model would end the turn, the gate re-injects the condition as a hidden user
message and forces another round instead of returning. The turn therefore runs
to completion across many rounds.

| Form | Effect |
|------|--------|
| `/pursue <condition>` | Set the condition, arm the gate, and drive the turn until met |
| `/pursue` | Re-arm and drive on the existing active pursuit |

The pursuit stops when:

- the model emits `[NEENEE_PURSUIT_COMPLETE]`;
- the 50-round safety cap is hit (the gate disarms);
- the user presses `Esc` or runs `/pursue stop`;
- a newer request supersedes it;
- the provider or tool pipeline returns an error.

This replaces the old outer multi-turn `/loop` (ADR-0009's uncapped loop) with
within-turn continuation — one driver, no outer loop. The clock-driven
counterpart is `/repeat`, a cron scheduler; see
[Pursuits](pursuits.md) for the comparison.

Task generation ids prevent an older task from clearing the cancellation state
of a newer task.

## Provider retry

Transient HTTP 408, 429, 5xx, connection, and timeout failures are retried up
to `provider_retry_max_attempts` (default 4, hard maximum 10). Provider
`Retry-After` or `retry-after-ms` headers take priority; otherwise the delay is
bounded exponential backoff using `provider_retry_base_ms` and
`provider_retry_max_ms`.

The TUI shows the next attempt and countdown without adding transcript noise.
`Esc`, `/pursue stop`, session switching, or a newer request cancels the wait.
Partial streamed assistant text is withdrawn before retry. Once any tool call
event has occurred, retryable errors become terminal so side effects are never
replayed.

## Safety bounds

- 3 consecutive identical tool calls.
- 8 seconds to initialize an MCP server.

Distinct tool calls and autonomous loop iterations are both **uncapped**,
matching the codex / claude-code agentic-loop model. Context compaction
(thresholds derived from the active model's context window, plus mid-turn
pruning) is the backstop that keeps unbounded loops from exhausting the
context window; the user can interrupt at any time with `Esc` or
`/pursue stop`. See ADR-0009 for the rationale and the prior caps (32 tool
rounds per turn, 50 autonomous iterations per `/loop`) that this decision
removed.

### Session review (ADR-0016)

Because an uncapped loop can still *appear* stuck (distinct-but-unproductive
tool calls that loop without converging), the harness runs a periodic
**session-review** diagnostic on long turns — a smarter, non-terminating
replacement for the old read-only "stall detector" that ADR-0009's uncapping
made redundant:

- After `[agent.review] review_start_round` (default **64**) tool rounds in a
  turn, and every `review_interval_rounds` (default **16**) thereafter, the
  harness spawns a bounded read-only diagnostic sub-agent (the `REVIEW`
  profile) that reads a compact snapshot of the live transcript and returns a
  verdict per registered review dimension.
- The worst verdict is surfaced as a visible activity-bar alert (empty verdict
  = clear). An explicit **stuck** verdict also pushes a one-shot hidden
  reflection nudge so the model gets a chance to recover.
- Review **never aborts the turn**. The only execution cap is an explicit,
  opt-in `hard_stop_rounds` (default **0** = off); a finite value is a
  user-declared budget and the sole thing that hard-stops a turn.
- "Is the agent looping?" is the first dimension (`LoopingReview`); adding more
  (context bloat, tool-error storms, …) is a `SessionReview` trait impl, no
  dispatch changes and no extra model call per dimension.
- Sub-agents (`subagent`) run with review **disabled**, so a short-lived
  read-only research sub-agent never pays for a diagnostic and review cannot
  recurse.

Configure or inspect live via the `/review` slash command
(`/review off`, `/review N [M]`, `/review default`).

These are execution bounds, not a security sandbox. Tool permission policy is
a separate future layer.

Write capability is enforced per-agent through a `WriteScope` boundary
(ADR-0028): the main agent is unrestricted (the permission broker is still the
interactive layer inside it); a subagent carries a scope resolved from its
profile, and a write tool whose target is outside that scope is blocked. All
built-in subagent profiles carry a `Read` ceiling today, so this gate is
inactive in practice but available to future scoped-write roles. MCP servers
with `read_only = false` declare `Write` and are subject to the same gate when
run inside a scoped subagent.

## Permission broker

Write-capable tools pass through a core permission broker before execution:

1. Core stores a one-shot waiter and emits `PermissionRequest`.
2. The CLI projects the request to the TUI.
3. The permission modal offers once, always, or reject.
4. Always requires a separate confirmation and is cached by tool plus resource
   scope for the current process. File writes scope by path and bash scopes by
   its complete command.
5. The reply resolves the waiter and tool execution resumes or returns a
   denied result.

Interrupting or superseding a task rejects all pending waiters and clears the
TUI blocker. `/permissions` makes cached rules observable and
`/permissions clear` revokes them.

The headless entry point automatically rejects write permissions.
Interactive clients use the event-driven entry point and reply to emitted
requests.

## Durable session

The CLI persists one active session as an atomic JSON snapshot and keeps
branch snapshots under `sessions/<id>.json`:

- Admission writes the visible or hidden user message before provider work.
- Agent execution uses a local message snapshot and does not hold the shared
  history mutex while waiting for providers, tools, or permissions.
- Commit replaces shared history and writes the full tool/assistant result.
- Startup restores visible messages, reconstructing native tool-call entries
  while filtering system and hidden harness prompts.
- `/session fork` creates a child with the same transcript and clears its loop
  checkpoint; `/session list` and `/session open <id-prefix>` allow branch
  navigation.
- Each turn records its admission session id and refuses a late commit after a
  session switch.

Loop checkpoints record pursuit, current iteration, and final status (the
iteration budget is uncapped — `usize::MAX` on the wire, see ADR-0009).
`/session status` exposes the checkpoint, `/resume` continues an
unfinished checkpoint, and `/session new` cancels old work and creates a
fresh session id.

## Context relief

The runner relieves context pressure in three layers, cheapest first. Every
threshold is derived from the **active model's context window** — measured in
tokens and re-seeded whenever the provider switches — so a 1M-token model is
no longer over-compacted at ~3% of its window and a 128k model is no longer
under-protected (ADR-0019). All three commit through one durable
archive-and-replace mechanism (`ContextRelief*`), so the complete transcript
survives while only the model-visible prefix is replaced.

| Layer | Trigger | Surfaced? |
|-------|---------|-----------|
| [Tool-result pruning](context-pruning.md) | ~65% of the window (`prune_utilization`) | Implicit — `debug` trace only |
| [Summarizing compaction](context-compaction.md) | ~85% of the window (`utilization`) | Visible — `Compacted` notice; `/compact` runs it manually |
| Overflow recovery | a provider reports context overflow | Reactive (see below) |

The first two layers each have a dedicated deep-dive — [Context
pruning](context-pruning.md) and [Context compaction](context-compaction.md);
exact keys and defaults live in the
[Configuration Reference](../../reference/configuration.md#compaction).

**Overflow recovery** is the harness's own reactive backstop and has no separate
page. If a provider reports context overflow *before* any `ToolCall` event, the
runner may compact and retry the same logical turn once. Overflow *after* tool
activity is terminal, so tool side effects are never replayed.

## Extension surfaces

- Skills add on-demand model instructions.
- MCP servers add dynamically discovered tools.
- Built-in tools and MCP tools share the `Tool` trait and event pipeline.
- Future permissions should wrap tool execution in the shared execution path.
- Future durable sessions should persist messages and loop checkpoints without
  changing the provider abstraction.
