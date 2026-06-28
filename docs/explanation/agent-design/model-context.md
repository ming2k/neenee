# Model Context

The model context is the request-scoped view neenee sends to a provider. It is
derived from the durable session, but it is not the durable session itself. The
provider receives only the current projection: a rebuilt system prompt, the
model window, and the tool catalog valid for this round.

For the persistent side of the same boundary, see
[Session persistence](session-persistence.md). For the prompt assembly rules
that feed this context, see [Prompt and message assembly](prompt-assembly.md).

## Stateless Provider Requests

LLM providers are treated as stateless. Every round sends enough context for the
provider to answer without relying on memory from an earlier HTTP request. The
next round resends the current model window with any newly appended assistant
messages and tool results.

That is why the "context sent to the LLM" is a projection rather than a storage
layer. It is assembled for a single provider call, serialized into that
provider's wire shape, and discarded after the response stream finishes. The
durable session keeps the recoverable state; the request body is only the
provider-facing materialization of that state.

## What the Request Contains

Each provider request carries three conceptual inputs:

| Input | Source | Purpose |
|-------|--------|---------|
| **System prompt** | Rebuilt from prompt sections and live state | Identity, behavior, active pursuit, enabled skill catalog, and conditional tool guidance |
| **Messages** | Current model window | User messages, assistant replies, assistant tool calls, tool results, and hidden harness messages that still belong in model-visible history |
| **Tools** | Current tool catalog | Native tool declarations: name, description, and parameter schema for each enabled tool |

The system prompt is rebuilt for every request. Tool schemas are also sent on
every request when the provider supports native tool calling. A disabled or
masked tool is not declared to the provider, and dispatch rejects calls to tools
that are not admitted for the current agent.

## Tool Schemas and Tool Trace

Tool definitions and tool usage are separate parts of the context.

The **tool schema** tells the model what it may call. It includes the tool name,
human-readable description, and structured parameter schema. This is the
declaration surface; it is resent every round because the provider is stateless.

The **tool trace** lives in messages. When the model calls a tool, the assistant
message records the tool name and JSON arguments. A read tool call can therefore
include parameters such as `offset` and `limit` as part of the assistant
tool-call arguments. After local execution, neenee appends a tool-result message
with the matching tool-call id and the result content. On the next request, both
the call and the result are present in the model window unless a later context
projection has shortened them.

This is the answer to the common question: yes, a later provider request can
contain the tool description, the fact that a read happened with its arguments,
and the tool result. They enter through different channels: declarations in the
tool catalog, usage and results in the message window.

## Native and Fallback Providers

Provider transports serialize the same conceptual context differently.

OpenAI-compatible providers receive a message array plus a separate native tool
schema field. Other providers may map messages and tools into a different
native shape, or may not support native function calling. When native tool
calling is unavailable, neenee uses the fallback path: the model emits a
tool-call-shaped text response, the harness promotes it into the structured tool
trace, then execution and result recording follow the same path as native tool
calls.

Before serialization, provider adapters may filter messages that violate the
target protocol. For example, a tool-result message whose tool-call id no longer
has a preceding assistant call is not sent. The durable session can preserve
more history than a specific provider wire format can accept; the request view
must stay valid for the selected transport.

## Projection Before Sending

The model context is always bounded by the current model window. If context
pressure rises, pruning and compaction project the durable session into a
smaller provider-visible window:

- pruning may remove large stale tool-result bodies while keeping the call/result
  structure,
- compaction may replace older complete turns with a checkpoint summary,
- both operations retain originals in the durable session,
- the next provider request sends the projected window, not the archived
  originals.

The projection therefore affects what the model reads, not what the session can
recover. This is the central boundary: the model context is optimized for the
next provider call; the durable session is optimized for full resume and audit.

## Mental Model

One round can be read as this flow:

```text
durable session
  -> restore current model window
  -> rebuild system prompt from live state
  -> declare currently enabled tools
  -> serialize provider-specific request
  -> stream assistant response
  -> append assistant/tool trace back into durable session
```

The arrows are one-way for the request. The provider does not update memory on
its own. Only the local session commit makes new messages durable and eligible
for the next model context.

