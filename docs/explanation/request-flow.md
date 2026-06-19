# Request flow

A user turn is a sequence of HTTP transactions driven by the ReAct loop.
This page documents the byte-level shape of each transaction and how the
message array evolves across the loop.

For the tool protocol that decides *when* a tool call appears in a
response, see [Tool lifecycle](tool-lifecycle.md). For the high-level turn
steps, see [Harness architecture](harness.md). For which providers speak
this contract, see [Providers](../reference/providers.md).

## One HTTP transaction

Every round of the loop is one independent HTTP request to the provider's
chat completions endpoint. The provider is stateless across rounds; the
full conversation history is re-sent each time.

### Request shape

```text
POST /v1/chat/completions HTTP/1.1
Authorization: Bearer <key>
Content-Type: application/json

{
  "model": "<model-id>",
  "stream": true,
  "messages": [
    {"role": "system",    "content": "<harness system prompt>"},
    {"role": "user",      "content": "<user prompt>"},
    {"role": "assistant", "content": "...", "tool_calls": [{"id": "...", "type": "function", "function": {"name": "...", "arguments": "..."}}]},
    {"role": "tool",      "tool_call_id": "...", "content": "<tool result>"}
  ],
  "tools": [<full schema set>],
  "tool_choice": "auto"
}
```

The body is assembled by `OpenAiCompatProvider::request_body`
(`crates/neenee-core/src/providers.rs`). Two fields are conditional:

| Field | When present | Source |
|-------|--------------|--------|
| `tools` | cached schema set is non-empty | `request_body` (`if let Some(tools) …` guard) |
| `tool_choice` | same condition as `tools` | `request_body` |

When the provider has no native function calling (`GeminiProvider`,
`LlamaServerProvider`), neither field is sent and the body uses a
different shape. See [Tool lifecycle](tool-lifecycle.md) for the fallback.

Orphan `tool` messages whose `tool_call_id` has no matching preceding
assistant `tool_calls` are filtered before the body is serialized inside
`request_body`. This keeps the runtime contract satisfied on restored or
forked sessions.

### Response shape

```text
HTTP/1.1 200 OK
Content-Type: text/event-stream
Transfer-Encoding: chunked

data: {"choices":[{"delta":{"role":"assistant"}}]}
data: {"choices":[{"delta":{"content":"Let me"}}]}
data: {"choices":[{"delta":{"content":" read"}}]}
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/lib.rs\"}"}}]}}]}
data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}
data: [DONE]
```

The response is **one** HTTP message. `Transfer-Encoding: chunked` lets the
server flush each `data:` line as it is generated; the client does not wait
for the body to complete before reading. This is standard HTTP/1.1 (or
HTTP/2 streaming) and is the mechanism OpenAI-compatible servers use to
stream tokens.

neenee reads the stream via `reqwest::Response::bytes_stream()` and splits
on newlines inside `OpenAiCompatProvider::stream_chat_events`
(`crates/neenee-core/src/providers.rs`). Each `data:`-prefixed line is one
SSE event. The literal `data: [DONE]` terminates the stream.

### SSE event shapes

Each `data:` payload is parsed by `parse_openai_stream_data`
(`crates/neenee-core/src/providers.rs`). The parser reads three optional
fields from `choices[0].delta` and emits a typed event for each non-empty
field:

| Delta field | Rust event (`ProviderStreamEvent`) | Reconstructed into |
|-------------|------------------------------------|--------------------|
| `content` | `TextDelta(String)` | assistant visible text |
| `reasoning_content` | `ReasoningDelta(String)` | reasoning text |
| `tool_calls[]` | `ToolCallDelta { index, id, name, arguments }` | tool calls |

A single delta may carry any combination of the three. A delta with an
empty `content` and no `tool_calls` produces no event. The terminal chunk
usually carries `finish_reason` (`stop`, `tool_calls`, `length`) and an
empty `delta`.

The `tool_calls` array in a delta is sparse: one SSE line may contain only
`index: 0` with an `id` and `name`, the next line only `index: 0` with a
fragment of `arguments`, and a later line `index: 1` for a second call. The
agent never assumes that a single delta contains a complete call.

## Tool call reassembly

Tool calls arrive as fragments keyed by `delta.tool_calls[].index`. A
single call may be split across many SSE events: the first fragment
typically carries `id` and `function.name`; subsequent fragments carry
pieces of `function.arguments` that must be concatenated.

The streaming loop inside `Agent::run_streaming_with_events`
(`crates/neenee-core/src/lib.rs`) maintains a `Vec<ToolCall>` that starts
empty and grows dynamically as higher `index` values appear:

```rust
while calls.len() <= index {
    calls.push(ToolCall {
        id: String::new(),
        name: String::new(),
        arguments: String::new(),
    });
}
let call = &mut calls[index];
if let Some(id) = id { call.id.push_str(&id); }
if let Some(name) = name { call.name.push_str(&name); }
call.arguments.push_str(&arguments);
```

`index` is `usize`; the vector is expanded to `index + 1` so that parallel
calls can be assembled in any interleaving order. Only the `arguments`
field is guaranteed to be present in every fragment; `id` and `name` are
`Option<String>` because some providers emit them only in the first
fragment.

Reassembly completes only when the stream ends. After `data: [DONE]` the
agent performs three cleanup steps before any side effects occur:

```text
calls.retain(|c| !c.name.is_empty())      // drop empty placeholder slots
for call in calls { if call.id.is_empty() { call.id = "call_<uuid>" } }
build assistant Message { role: Assistant, content, tool_calls: Some(calls) }
messages.push(response); then execute tool calls
```

Slots with an empty `name` are discarded because some providers emit
zero-valued `tool_calls` deltas. Empty `id` fields are backfilled with
`call_<uuid>` so that the following `tool` message has a valid
`tool_call_id` to reference.

Side effects never fire mid-stream. This is what makes retry safe: a stream
that errors before `[DONE]` can be re-issued without leaving partial tool
state behind. Once any tool has executed, retryable errors become terminal
(see [Retry interaction](#retry-interaction)).

## The ReAct loop

The loop runs in `Agent::run_streaming_with_events`
(`crates/neenee-core/src/lib.rs`) for interactive turns and
`Agent::run_with_events` for headless turns. The structure is identical;
only the transport differs.

```mermaid
flowchart TD
    A["prepare_tools"] --> B{"tool_rounds ≥ 32?"}
    B -- yes --> X["abort: too many rounds"]
    B -- no --> C["ensure_system_prompt"]
    C --> D["stream_chat_events"]
    D --> E["accumulate text / reasoning / tool_calls"]
    E --> F["push assistant message"]
    F --> G{"response has<br/>tool_calls?"}
    G -- yes --> H["guard_repeated_call"]
    H -- I["execute_tool — local, no HTTP"]
    I --> J["push tool result"]
    J --> K["tool_rounds += 1<br/>continue → next HTTP request"]
    K --> B
    G -- no --> L{"fallback JSON<br/>parses?"}
    L -- yes --> M["attach_fallback_tool_call"]
    M --> N["execute_tool; push result"]
    N --> B
    L -- no --> O["return response — loop exits"]
```

### Tool dispatch

The branching after the assistant message is handled by
`Agent::dispatch_tool_calls` (`crates/neenee-core/src/lib.rs`). For a native
tool-call round it emits one `ToolCall` event per call up front, executes all
calls concurrently with `execute_tools_concurrent`, and then records the
results in input order. For a text-fallback round it parses the assistant
`content` as JSON, optionally emits `AssistantDiscard` to retract the raw JSON
from the UI, promotes the parsed call onto the assistant message with
`attach_fallback_tool_call`, and executes a single call.

In both cases the actual side-effecting work is performed by
`Agent::execute_tool`, which handles lookup, Plan-mode gating, the permission
broker, and the final `Tool::call_with_events` invocation.

### Messages evolution

The model has no memory between requests. What it "knows" about prior
tool calls is entirely a function of the `messages` array neenee
re-sends each round. A turn that reads a file, edits it, and summarizes
produces three HTTP transactions:

**Request 1** — the user turn opens the loop.

```text
messages: [
  {role: system,    content: "<harness system prompt>"},
  {role: user,      content: "Fix the bug in parser.rs and explain it"}
],
tools: [<all schemas>]
```

Response carries `tool_calls: [read_file("src/parser.rs")]`,
`finish_reason: "tool_calls"`. neenee executes `read_file` locally and
appends the result.

**Request 2** — same endpoint, expanded history.

```text
messages: [
  {role: system,    content: "<harness system prompt>"},
  {role: user,      content: "Fix the bug in parser.rs and explain it"},
  {role: assistant, content: "I'll read the file first.",
                    tool_calls: [{id: "call_1", function: {name: "read_file", arguments: "{\"path\":\"src/parser.rs\"}"}}]},
  {role: tool,      tool_call_id: "call_1", content: "<file contents>"}
],
tools: [<all schemas>]   ← same set, re-sent verbatim
```

Response carries `tool_calls: [edit_file(...)]`. neenee executes the
edit and appends the result.

**Request 3** — history now contains two tool rounds.

```text
messages: [
  ...,
  {role: assistant, tool_calls: [{id: "call_2", function: {name: "edit_file", arguments: "..."}}]},
  {role: tool,      tool_call_id: "call_2", content: "<edit applied>"}
],
tools: [<all schemas>]
```

Response carries plain text `content: "The bug was ..."`,
`finish_reason: "stop"`. No `tool_calls` field. The loop exits and the
assistant message becomes the turn's final answer.

The `tools` array is byte-identical across all three requests. The
`messages` array grows monotonically; neenee never edits prior messages
(except the fallback promotion described in
[Tool lifecycle](tool-lifecycle.md)).

### Exit conditions

The loop returns a final assistant message when any of these holds:

| Condition | Where | Result |
|-----------|-------|--------|
| Response has no `tool_calls` and no fallback JSON parses | `run_streaming_with_events` tail | Success; assistant text is the answer |
| `tool_rounds >= MAX_TOOL_ROUNDS` (32) | `run_streaming_with_events` head | Error; turn aborts |
| `guard_repeated_call` rejects a 4th consecutive identical call | `guard_repeated_call` | Error; turn aborts |
| Provider or tool pipeline returns an error | propagated | Error; turn aborts |
| Context overflow before any tool event | retry layer | Compact and retry once |

### Safety bounds

Two bounds prevent runaway loops (`crates/neenee-core/src/lib.rs`):

- `MAX_TOOL_ROUNDS = 32`. A single turn cannot exceed 32 tool rounds
  (each round may contain multiple parallel tool calls).
- `MAX_REPEATED_TOOL_CALLS = 3`. `guard_repeated_call` tracks the previous
  `(name, arguments)` pair. After three consecutive identical calls the
  fourth is rejected with an error. Distinct calls and interleaved text
  resets the counter.

These are execution bounds, not a security sandbox. See
[Harness architecture](harness.md) for the full safety surface.

## Fallback variant

When the provider has no native function calling the response never
carries a `tool_calls` field. Instead the model is instructed to emit the
call as ordinary assistant text:

```text
{"tool": "read_file", "arguments": {"path": "src/parser.rs"}}
```

After the stream completes, `Agent::parse_tool_call`
(`crates/neenee-core/src/lib.rs`) extracts the JSON from the
assistant `content`. `Agent::attach_fallback_tool_call` then promotes the
parsed call onto the preceding assistant message as a synthetic
`tool_calls` entry, so the next request's `messages` array carries a valid
`tool_calls` / `tool_call_id` pair even though the original response was
plain text.

The resulting `messages` evolution is identical to the native path. The
only difference is whether the tool call arrives as a structured
`tool_calls` field or is parsed out of `content`. See
[Tool lifecycle](tool-lifecycle.md) for the parsing rules and their limits.

## Retry interaction

Retry lives at the turn level (inside `execute_turn` in
`crates/neenee/src/main.rs`), not inside the provider. A retryable failure
(HTTP 408, 429, 5xx, connection, timeout) is wrapped in `RetryableError`
and re-issued after backoff.

Two invariants shape the interaction between retry and the ReAct loop:

- **Pre-tool retry is safe.** If the stream errors before any tool has
  executed, the entire request can be re-issued. No side effects have
  occurred; the `messages` array is unchanged.
- **Post-tool retry is terminal.** Once `execute_tool` has run for any
  call in the current round, retryable errors become terminal
  (the `tool_activity` check in `execute_turn`). Re-issuing would risk
  replaying side effects (a second file write, a second shell command).

The deferred-execution rule from [Tool call reassembly](#tool-call-
reassembly) is what makes the first invariant hold. Because tools only
fire after `[DONE]`, a mid-stream failure always lands in the safe
pre-tool window.

Partial streamed assistant text is withdrawn from the visible transcript
before a retry so the user does not see a half-finished answer followed
by a fresh one.

## See also

- [Tool lifecycle](tool-lifecycle.md) — schema injection and fallback mechanics
- [Provider capabilities](provider-capabilities.md) — why providers differ
  on streaming and tool support
- [Harness architecture](harness.md) — turn execution, retry, safety bounds
- [Providers](../reference/providers.md) — endpoint catalog
