# Provider capabilities

Tool calling and reasoning are often described as "model capabilities." In
practice they are the product of three cooperating layers, and neenee
consumes them differently depending on which layers are present. This page
explains where each capability actually lives and why providers differ.

For the per-provider capability matrix, see
[Providers](../reference/providers.md). For the wire-level protocol neenee
uses to call tools, see [Tool rounds](agent-design/tool-rounds.md).

## Three layers

| Layer | Owns | Examples |
|-------|------|----------|
| Model weights | Behavior under tool-use prompts; whether reasoning is emitted at all | `deepseek-v4-flash`, `glm-5.2`, `kimi-k2.7-code`, `gemini-2.0-flash` |
| Serving runtime | HTTP API shape, `tools` / `tool_choice` field parsing, guided JSON decoding, SSE chunking, `reasoning_content` field passthrough | vLLM, SGLang, TGI, TensorRT-LLM, and the hosted gateways (`api.openai.com`, `api.deepseek.com`, `open.bigmodel.cn`, Moonshot, Volcengine Ark) |
| Client (neenee) | Schema declaration, delta reconstruction, fallback parsing, registry, permission brokering | `crates/neenee-core` |
A tool call only succeeds when all three layers agree. A model whose weights
were never tool-tuned will emit free text even if the runtime accepts a
`tools` field; a runtime without guided decoding may return malformed JSON
even from a tool-tuned model; a client that fails to reassemble `delta`
fragments by `index` will drop calls mid-stream. neenee's design assumes the
serving runtime implements the OpenAI Chat Completions contract and degrades
gracefully when it does not.

## Function calling is not native to weights

A base language model produces token sequences. "Calling a tool" is a
discipline imposed on top of that:

1. The serving runtime injects the client-supplied tool schemas into the
   prompt using the model's chat template (every model family has its own
   tool-use prompt format — Hermes, Llama-3, Qwen, GLM, etc.).
2. The runtime applies guided decoding (vLLM uses `outlines` or `xgrammar`;
   SGLang and TGI have equivalents) to constrain generation so the model
   emits a parseable `tool_calls` JSON structure instead of prose.
3. The runtime exposes the result through the OpenAI-compatible
   `choices[].message.tool_calls` field, or as `delta.tool_calls[]`
   fragments in SSE.

The model weights decide *which* tool to call and *what arguments* to write;
the runtime decides *whether* the output is structured as a tool call at all.
This is why two servings of the same weights (for example a raw vLLM instance
without a tool template versus the vendor's hosted endpoint) can behave
differently on the same `tools` payload.

neenee trusts the runtime to deliver well-formed OpenAI-shaped tool calls.
The `OpenAiCompatProvider` declares schemas via `prepare_tools`
(`crates/neenee-core/src/providers.rs`) and injects them into every
request body through `request_body`. It does not implement its own guided
decoding or prompt templating — that is the runtime's job. For the
mechanics of constrained decoding and chat templates, see
[Guided decoding](guided-decoding.md).

## Reasoning is a passthrough

The `reasoning_content` field that some models emit is not requested through
an API flag. It is produced by the model weights (reasoning-tuned variants
such as DeepSeek V4 thinking mode, Qwen reasoning models, GLM reasoning models)
and passed through verbatim by the serving runtime as a sibling of `content`
in the response object.

neenee never declares reasoning support and never sends a flag that would
enable it. `parse_openai_stream_data` and the non-streaming `chat` parser
(both in `providers.rs`) simply observe the field when it is present and
forward it as `ProviderStreamEvent::ReasoningDelta`. A non-reasoning model
produces no such events; no capability negotiation is involved.

This makes reasoning cheap to consume but also impossible to enable from the
client side. Using a reasoning-tuned model variant (e.g. DeepSeek V4 with
thinking mode enabled) is what turns reasoning on, not any neenee setting.

## Streaming is a runtime contract

SSE chunking is part of the OpenAI Chat Completions contract that serving
runtimes implement. Two runtime behaviors matter to neenee:

- **Delta fragmentation.** The runtime is allowed to split a single tool
  call across many SSE chunks, indexed by `delta.tool_calls[].index`. neenee
  reassembles them by index in the streaming loop inside
  `Agent::run_streaming_with_events` (`crates/neenee-core/src/lib.rs`) and
  does not execute a tool until the stream terminates.
- **Field selection.** A runtime may omit `reasoning_content` or
  `tool_calls` entirely from deltas where they have no new data. neenee's
  parser treats every delta field as optional.

Providers that do not implement `stream_chat_events` (Gemini,
LlamaServer) fall back to the trait default, which wraps `stream_chat` and
emits only `TextDelta` events. They cannot surface reasoning or stream
tool-call deltas even when the underlying service might support them.

## Why providers differ

neenee's provider adapters encode an opinionated mapping between the three
layers:

- **OpenAI-compatible registry presets** (`kimi-code`, `deepseek-v4-flash`,
  `deepseek-v4-pro`, `zai-code`, plus the bespoke `openai`
  entries, all backed by `OpenAiCompatProvider`) assume a runtime that fully
  implements the OpenAI Chat Completions contract including `tools`,
  `tool_choice`, `reasoning_content`, and SSE tool-call deltas. The
  registry presets are pure data in `OPENAI_PROVIDER_SPECS`;
  `OpenAiProviderSpec::build` constructs an `OpenAiCompatProvider` for each, so
  they inherit every capability from one shared implementation.
- **Gemini** (`GeminiProvider`) speaks a different request shape
  (`systemInstruction`, `model`/`user` roles, no `tools` field). neenee does
  not bridge Gemini's native function-calling API; tool calls fall through
  to the universal text protocol.
- **LlamaServer** (`LlamaServerProvider`) targets an OpenAI-compatible
  endpoint but does not implement `prepare_tools` or `stream_chat_events`.
  The adapter treats the server as a text-only channel even when the
  underlying runtime (typically vLLM) could accept `tools`. Tool calls
  therefore fall through to the universal text protocol.

The practical consequence: on Gemini and LlamaServer the model must emit
`{"tool": "<name>", "arguments": {…}}` as ordinary assistant text, which
`Agent::parse_tool_call` (`crates/neenee-core/src/lib.rs`) extracts
after the fact. See [Tool rounds](agent-design/tool-rounds.md) for the fallback
mechanics.

## Capability negotiation summary

| Capability | Negotiated? | Source of truth |
|------------|-------------|-----------------|
| Tool schemas | Declared by client on every request | `prepare_tools` + request body injection |
| Tool selection | Model weights decide | `tool_choice: "auto"` lets the model pick |
| Structured tool output | Runtime guided decoding | Serving runtime (vLLM, hosted gateway, etc.) |
| Reasoning | Not negotiated | Model weights emit; runtime passes through; client observes |
| SSE delta fragmentation | Runtime contract | OpenAI-compatible streaming protocol |
| Fallback text protocol | Client-side | `parse_tool_call` on assistant content |

## See also

- [Providers](../reference/providers.md) — per-provider capability matrix
- [Tool rounds](agent-design/tool-rounds.md) — schema injection, streaming, fallback
- [Built-in tools](../reference/tools/index.md) — what schemas get declared
- [Harness architecture](agent-design/harness.md) — how the harness consumes these
  capabilities per turn
