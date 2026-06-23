# Harness Architecture

The harness is the control plane around provider calls. It keeps model output
inside explicit state, execution, and safety boundaries.

## Turn execution

Every CLI turn runs the streaming agent loop:

1. Refresh the system context with mode, goal, tools, and skill metadata.
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
[Tool rounds](tool-rounds.md).

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

## Goal state

`/goal <objective>` creates a durable, per-session objective persisted in
SQLite (`goals.db`) keyed by thread id, so it survives restarts and `/resume`.
A goal is a slim primitive: an objective, an in-memory checklist, and a
single `is_complete` flag (no status machine, no token or time budget — both
were removed in ADR-0010). The model drives goals through `get_goal`,
`create_goal`, `update_goal`, and `goal_checklist`, and signals completion
with `[NEENEE_GOAL_COMPLETE]` — which the harness defers while any checklist
item remains pending. See [Goals](goals.md) for the primitive, the checklist
rules, and the persistence model.

## Autonomous loop

`/loop` runs an uncapped autonomous loop driving the active goal. Each
iteration is a complete agent turn with current filesystem state and
conversation history, preceded by a hidden control prompt that re-states
the goal and asks the model to keep making concrete progress.

| Form | Effect |
|------|--------|
| `/loop` | Start on the active goal (set one with `/goal <objective>` first) |
| `/loop <objective>` | Set a fresh goal and start the loop in one step |
| `/loop resume` | Resume an unfinished durable checkpoint |

The loop stops when:

- the completion marker is emitted (and the goal checklist allows completion);
- the user presses `Esc` or runs `/loop stop`;
- a newer chat or loop request supersedes it;
- the provider or tool pipeline returns an error.

There is no iteration budget. The previous `1..=50` cap was removed
(ADR-0009) to align with the codex / claude-code agentic-loop model: the
loop runs until the model itself stops calling tools, with context
compaction as the backstop that keeps long loops bounded. A legacy
`/loop <N>` form is rejected with a migration hint.

Task generation ids prevent an older task from clearing the cancellation state
of a newer task.

## Provider retry

Transient HTTP 408, 429, 5xx, connection, and timeout failures are retried up
to `provider_retry_max_attempts` (default 4, hard maximum 10). Provider
`Retry-After` or `retry-after-ms` headers take priority; otherwise the delay is
bounded exponential backoff using `provider_retry_base_ms` and
`provider_retry_max_ms`.

The TUI shows the next attempt and countdown without adding transcript noise.
`Esc`, `/loop stop`, session switching, or a newer request cancels the wait.
Partial streamed assistant text is withdrawn before retry. Once any tool call
event has occurred, retryable errors become terminal so side effects are never
replayed.

## Safety bounds

- 3 consecutive identical tool calls.
- 8 seconds to initialize an MCP server.

Distinct tool calls and autonomous loop iterations are both **uncapped**,
matching the codex / claude-code agentic-loop model. Context compaction
(`compaction_max_chars` plus mid-turn pruning) is the backstop that keeps
unbounded loops from exhausting the context window; the user can interrupt
at any time with `Esc` or `/loop stop`. See ADR-0009 for the rationale and
the prior caps (32 tool rounds per turn, 50 autonomous iterations per
`/loop`) that this decision removed.

These are execution bounds, not a security sandbox. Tool permission policy is
a separate future layer.

Plan mode is enforced per-invocation through each tool's plan-mode access
check, which
defaults to read-only access and exempts writes under `.neenee/plans/`. MCP
servers are therefore blocked in Plan mode unless their config explicitly sets
`read_only = true`. The model can also switch modes itself via the injected
`plan_enter`/`plan_exit` tools; see [Plan mode](plan-mode.md) for the full
workflow and the write exemption.

## Permission broker

Write-capable Build-mode tools pass through a core permission broker before
execution:

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

Loop checkpoints record goal, current iteration, and final status (the
iteration budget is uncapped — `usize::MAX` on the wire, see ADR-0009).
`/session status` exposes the checkpoint, `/loop resume` continues an
unfinished checkpoint, and `/session new` cancels old work and creates a
fresh session id.

## Context compaction

The runner relieves context pressure in three layers, cheapest first:

1. **Tool-result pruning** (`compaction_prune`, on by default). Old tool-role
   results are cleared in place to `[Old tool result content cleared]`,
   protecting the most recent `compaction_prune_protect_chars` of tool output.
   This runs both before a turn and, via a mid-turn relief gate, between
   tool rounds once pressure crosses ~¾ of `compaction_max_chars`. Pruned
   originals are archived for durability; the `tool_call_id` chain is preserved
   so providers that require it stay valid.
2. **Summarizing compaction** when size still exceeds `compaction_max_chars`.
   The boundary is the start of an older complete user turn:
   - Earlier messages move to the durable archive.
   - System messages are regenerated rather than archived into model context.
   - When `compaction_summarize` is on (default), the active model writes an
     anchored, structured summary; the previous summary is carried forward in a
     `<previous-summary>` block so each compaction updates rather than restarts.
     Any failure falls back to a deterministic newest-first excerpt summary.
   - The latest `compaction_preserve_turns` remain provider-native.

This preserves the complete transcript while replacing only the model-visible
prefix. `/compact` runs the same operation manually.

If a provider reports context overflow before any ToolCall event, the runner
may compact and retry the same logical turn once. Overflow after tool activity
is terminal so tool side effects are never replayed.

## Extension surfaces

- Skills add on-demand model instructions.
- MCP servers add dynamically discovered tools.
- Built-in tools and MCP tools share the `Tool` trait and event pipeline.
- Future permissions should wrap tool execution in the shared execution path.
- Future durable sessions should persist messages and loop checkpoints without
  changing the provider abstraction.
