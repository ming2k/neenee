# neenee-providers

Concrete LLM provider implementations and the `build_provider_for_channel`
factory consumed by the orchestration layer.

Each transport lives in its own module:

- `mock` — trivial in-memory provider used as the default channel;
- `openai_compat` — OpenAI-compatible chat completions with native tool calls
  and a streaming filter that strips tool-call "echo" text;
- `anthropic_compat` — Anthropic-compatible `/messages` (MiniMax/Qwen and any
  Anthropic-format relay);
- `gemini` — Google Gemini native REST surface;
- `registry` — the table of OpenAI-compatible endpoints, the configurable
  `anthropic` Claude relay model list, and `build_provider_for_channel`, the
  single place that turns a `neenee_core::catalog::Channel` into a concrete
  `dyn Provider`;
- `sse` — the shared Server-Sent Events byte-stream decoder every streaming
  transport reuses.

A keyless OpenAI-compatible relay reaches the same `OpenAiCompatProvider` as a
cloud endpoint (an empty key suppresses the auth header), so there is no
separate local provider module.
