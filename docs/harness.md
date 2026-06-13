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

## Goal state

`/goal <objective>` creates an active goal and persists it in config. The goal
is injected on every turn. `/goal done` marks it completed and `/goal clear`
removes it.

The completion marker `[NEENEE_GOAL_COMPLETE]` is a model-to-harness control
signal. It is removed from visible output and only used to end an autonomous
loop early. A structured goal checklist is maintained through the built-in
`goal_checklist` tool. The completion marker is deferred while any item remains
pending or in progress.

Checklist updates are harness metadata: they do not require a write permission
prompt, but they are validated, persisted in config, injected into subsequent
system prompts, and projected into the TUI as `done/total` progress. A
populated active checklist cannot be replaced with an empty list; each item
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

Plan mode uses `ToolAccess`, not tool names. Built-in read tools are explicitly
marked read-only; all other tools default to write-capable. MCP servers are
therefore blocked in Plan mode unless their config explicitly sets
`read_only = true`.

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
