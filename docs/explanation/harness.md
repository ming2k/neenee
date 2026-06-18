# Harness Architecture

The harness is the control plane around provider calls. It keeps model output
inside explicit state, execution, and safety boundaries.

## Turn execution

Every CLI turn uses `Agent::run_streaming_with_events()`:

1. Refresh the system context with mode, goal, tools, and skill metadata.
2. Stream provider text and reconstruct native tool-call deltas by index.
3. Execute native or JSON fallback tool calls through the same registry.
4. Emit tool call/result events for the TUI.
5. Stop on a final assistant message or a harness safety bound.

Streaming remains inside the harness. Text fallback JSON is withdrawn from the
visible transcript before its tool card is emitted.

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
see [Provider capabilities](provider-capabilities.md) and
[Tool protocol](tool-protocol.md).

### Declared: tools

Tool schemas live inside the provider, not the conversation. Each turn calls
`provider.prepare_tools(&self.tools)` before any network work, caching every
tool's OpenAI function schema (`Tool::to_openai_function()`) in
`OpenAiCompatProvider.tools`. Every HTTP request then re-injects the full cached
set:

```text
body["tools"]       = all cached schemas
body["tool_choice"] = "auto"
```

Tool schemas are request-scoped. Every ReAct round, including the round that
carries tool results back upstream, sends the same complete schema set
alongside the full message history. The provider is stateless across turns.

`OpenAiCompatProvider` declares schemas natively. The OpenAI-compatible registry
presets (`kimi-code`, `kimi`, `deepseek`, `qwen`, `glm`, `volcengine`) and
the bespoke `custom` entry all inherit the same path: each is built by
`OpenAiProviderSpec::build` (or `OpenAiCompatProvider::with_base_url` for
`custom`) into an `OpenAiCompatProvider`, so they delegate to its
`prepare_tools`. `GeminiProvider` and `LlamaServerProvider` do not override
the default `prepare_tools` and never send a `tools` field; tool calls on
those providers travel only through the universal fallback below.

### Observed: reasoning

The harness never declares reasoning support and never sends a flag that
requests it. Providers passively read `reasoning_content` from stream deltas
(`parse_openai_stream_data`) and complete messages (`chat`), forwarding it as
`ProviderStreamEvent::ReasoningDelta`. Only models that emit the field
(`deepseek-reasoner`, reasoning-tuned GLM and Qwen variants) surface
reasoning; other models produce no `ReasoningDelta` events.

Reasoning is rendering metadata. It is not summarized, not re-injected as
follow-up context, and not used for control flow.

### Tool call transport

Both execution paths feed one shared registry:

| Path | Transport | Tool calls |
|------|-----------|-----------|
| `chat()` | Single HTTP round trip | `choices[0].message.tool_calls` complete |
| `stream_chat_events()` | SSE stream | `delta.tool_calls` fragments accumulated by `index` |

The streaming path accumulates `id`, `name`, and `arguments` per index while
text and reasoning deltas render live. After the stream reaches `[DONE]`,
calls with an empty `id` are assigned `call_<uuid>`, calls with an empty
`name` are dropped, and the survivors are handed to `execute_tool`. Side
effects never fire mid-stream.

### Universal fallback

For providers without native function calling, `Agent::parse_tool_call()`
extracts `{"tool": "<name>", "arguments": {…}}` from assistant text.
`Agent::attach_fallback_tool_call()` then promotes the parsed call onto the
preceding assistant message as a native `tool_calls` entry so
OpenAI-compatible `tool_call_id` pairing stays valid on the next round.

Fallback text is withdrawn from the visible transcript before the tool card
is emitted, matching the native streaming path. The same registry, permission
broker, and result-message format apply to native and fallback calls.

## Goal state

`/goal <objective>` creates an active goal. Goals are persisted per session in a
SQLite store (`goals.db`) keyed by thread id, so they survive restarts and are
restored on `/resume`; a one-time migration imports any legacy goal from the
old config. The goal is injected on every turn. `/goal done` marks it completed,
`/goal clear` removes it, and `/goal pause`, `/goal resume`, `/goal edit`, and
`/goal budget` manage its lifecycle.

A goal carries a status, not just a done flag. Transitions are validated by the
goal service:

| Status | Meaning |
|--------|---------|
| `active` | Being worked on; injected into the system prompt each turn |
| `paused` | Suspended by the user; resumable |
| `blocked` | Model gave up after a blocking condition recurred; resumable |
| `usage_limited` / `budget_limited` | Stopped by a usage cap or token budget; resumable |
| `complete` | Objective achieved |

Each completed turn's token and elapsed-time cost is accounted against the
active goal. When a token budget is set and cumulative `tokens_used` reaches it,
the goal moves to `budget_limited` and a hidden prompt informs the model; the
user raises the budget with `/goal budget <tokens>` or continues with
`/goal resume`.

The model also drives goals through tools: `get_goal` (read), `create_goal`
(start a new goal on explicit request), `update_goal` (mark `complete` or
`blocked`), and `goal_checklist` (expose progress). The legacy completion marker
`[NEENEE_GOAL_COMPLETE]` remains a model-to-harness control signal: it is removed
from visible output and used to end an autonomous loop early, and is deferred
while any checklist item remains pending or in progress.

Checklist updates are harness metadata: they do not require a write permission
prompt, but they are validated, held in live goal state, injected into
subsequent system prompts, and projected into the TUI as `done/total` progress.
A populated active checklist cannot be replaced with an empty list; each item
must receive a terminal completed or cancelled status.

## Autonomous loop

`/loop` defaults to 8 iterations. `/loop <N>` accepts `1..=50`.
`/loop resume` retries the unfinished iteration from the durable checkpoint.
It restores the checkpoint goal and removes an incomplete trailing hidden loop
prompt before continuing. Completed and exhausted checkpoints are terminal.

Each iteration is a complete agent turn with current filesystem state and
conversation history. A loop stops when:

- the completion marker is emitted;
- the iteration budget is exhausted;
- the user presses `Esc` or runs `/loop stop`;
- a newer chat or loop request supersedes it;
- the provider or tool pipeline returns an error.

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

- 32 tool rounds per agent turn.
- 3 consecutive identical tool calls.
- 50 autonomous iterations per `/loop`.
- 8 seconds to initialize an MCP server.

These are execution bounds, not a security sandbox. Tool permission policy is
a separate future layer.

Plan mode is enforced per-invocation through `Tool::allowed_in_plan_mode`, which
defaults to read-only access and exempts writes under `.neenee/plans/`. MCP
servers are therefore blocked in Plan mode unless their config explicitly sets
`read_only = true`. The model can also switch modes itself via the injected
`plan_enter`/`plan_exit` tools; see [Plan mode](plan-mode.md) for the full
workflow and the write exemption.

## Permission broker

Write-capable Build-mode tools pass through a core permission broker before
`Tool::call()`:

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

`Agent::run()` is the headless-safe entry point and automatically rejects
write permissions. Interactive clients must use `run_with_events()` and reply
to emitted requests.

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

Loop checkpoints record goal, iteration budget, current iteration, and final
status. `/session status` exposes the checkpoint, `/loop resume` continues an
unfinished checkpoint, and `/session new` cancels old work and creates a fresh
session id.

## Context compaction

Before provider execution, the CLI estimates active request size by UTF-8
characters. If it exceeds `compaction_max_chars`, it chooses a boundary at the
start of an older complete user turn:

- Earlier messages move to the durable archive.
- System messages are regenerated rather than archived into model context.
- A hidden deterministic checkpoint summarizes bounded excerpts.
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
- Future permissions should wrap tool execution in `Agent::execute_tool()`.
- Future durable sessions should persist messages and loop checkpoints without
  changing the provider abstraction.
