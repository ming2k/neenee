# Providers

The agent talks to LLM providers through the `Provider` trait
(`crates/neenee-core/src/lib.rs:127`). Every provider implementation lives in
`crates/neenee-core/src/providers.rs`. Provider selection happens at startup
and on `/provider switch` in `crates/neenee/src/main.rs`.

## Capability matrix

Three capability surfaces matter for tool-using agents:

- **Native tools** — the provider overrides `prepare_tools` and injects a
  `tools` field into the request body. Without it, the agent falls back to
  the universal text protocol.
- **Reasoning** — the provider reads `reasoning_content` from responses and
  emits `ProviderStreamEvent::ReasoningDelta`.
- **Structured streaming** — the provider implements `stream_chat_events`
  with the full event set (`TextDelta`, `ReasoningDelta`, `ToolCallDelta`).
  Providers that do not implement it fall back to the trait default, which
  only emits `TextDelta`.

| Provider | Native tools | Reasoning | Structured streaming | Source |
|----------|--------------|-----------|----------------------|--------|
| `OpenAIProvider` | yes | yes | yes | `providers.rs:96` |
| `KimiCodeProvider` | yes | yes | yes | `providers.rs:702` |
| `KimiProvider` | yes | yes | yes | `providers.rs:746` |
| `DeepSeekProvider` | yes | yes | yes | `providers.rs:785` |
| `QwenProvider` | yes | yes | yes | `providers.rs:824` |
| `GLMProvider` | yes | yes | yes | `providers.rs:870` |
| `VolcengineProvider` | yes | yes | yes | `providers.rs:908` |
| `GeminiProvider` | no | no | no | `providers.rs:397` |
| `LlamaServerProvider` | no | no | no | `providers.rs:565` |
| `MockProvider` | no | no | no | `providers.rs:62` |

The six wrappers (`KimiCodeProvider` through `VolcengineProvider`) delegate
all four trait methods to a wrapped `OpenAIProvider`, so they inherit every
capability. `GeminiProvider` and `LlamaServerProvider` are standalone and
hard-code `tool_calls: None` and `reasoning_content: None` in their `chat`
implementations; tool calls on those providers travel only through the
universal fallback.

## Provider catalog

`default_provider` in `config.toml` selects the initial provider. The same
names are accepted by `/provider switch`. API keys may be supplied through
environment variables or `config.toml` fields; model selection uses a
separate `<NAME>_MODEL` env var.

| `default_provider` | Struct | Endpoint | API key env | Model env | Default / popular models |
|--------------------|--------|----------|-------------|-----------|--------------------------|
| `openai` | `OpenAIProvider` | `https://api.openai.com/v1/chat/completions` | `OPENAI_API_KEY` | `OPENAI_MODEL` | `gpt-4o`, `gpt-4o-mini` |
| `deepseek` | `DeepSeekProvider` | `https://api.deepseek.com/v1/chat/completions` | `DEEPSEEK_API_KEY` | `DEEPSEEK_MODEL` | `deepseek-chat`, `deepseek-reasoner` |
| `qwen` | `QwenProvider` | `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions` | `DASHSCOPE_API_KEY` | `QWEN_MODEL` | `qwen-plus`, `qwen-max`, `qwen-turbo`, `qwen-coder-plus` |
| `glm` | `GLMProvider` | `https://open.bigmodel.cn/api/paas/v4/chat/completions` | `GLM_API_KEY` | `GLM_MODEL` | `glm-4-plus`, `glm-4`, `glm-4-air`, `glm-4-flash`, `glm-4v` |
| `volcengine` | `VolcengineProvider` | `https://ark.cn-beijing.volces.com/api/v3/chat/completions` | `VOLCENGINE_API_KEY` | `VOLCENGINE_MODEL` | `deepseek-v3-250324`, `deepseek-r1-250324`, `doubao-pro-256k` |
| `kimi` | `KimiProvider` | `https://api.moonshot.cn/v1/chat/completions` | `KIMI_API_KEY` | `KIMI_MODEL` | `moonshot-v1-8k`, `moonshot-v1-32k`, `moonshot-v1-128k`, `moonshot-v1-8k-vision-preview` |
| `kimi-code` | `KimiCodeProvider` | `https://api.kimi.com/coding/v1/chat/completions` | `KIMI_CODE_API_KEY` | fixed `kimi-for-coding` | `kimi-for-coding` |
| `gemini` | `GeminiProvider` | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}` | `GEMINI_API_KEY` | `GEMINI_MODEL` | `gemini-1.5-pro`, `gemini-1.5-flash`, `gemini-2.0-flash` |
| `llama` | `LlamaServerProvider` | `${LLAMA_BASE_URL}/v1/chat/completions` | none | `LLAMA_MODEL` | user-supplied |
| `custom` | `OpenAIProvider` | `${CUSTOM_BASE_URL}` | `CUSTOM_API_KEY` | `CUSTOM_MODEL` | user-supplied |
| `mock` | `MockProvider` | n/a | none | none | test fixture |

Notes:

- `kimi-code` is the only wrapper that takes a `user_agent` argument
  (`KIMI_CODE_USER_AGENT`); the model ID is fixed at `kimi-for-coding`.
- `qwen` reads its API key from `DASHSCOPE_API_KEY` but its model from
  `QWEN_MODEL`. The international endpoint variant
  (`https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions`)
  is available through `QwenProvider::new_intl` but is not exposed through
  the `default_provider` switch.
- `llama` and `custom` are the only providers that read a base URL; the rest
  hard-code the endpoint inside `providers.rs`.
- `llama` and `mock` always report as ready in the API-key status check
  (`crates/neenee/src/main.rs:700, 705`); the rest require their API key env
  var to be set.

## Dispatch sites

Provider construction is centralized in `crates/neenee/src/main.rs`:

| Site | File:line | Purpose |
|------|-----------|---------|
| Startup dispatch | `main.rs:777-883` | `match config.default_provider.as_str()` |
| Runtime switch | `main.rs:1079-1160` | `AgentRequest::SwitchProvider` handler |
| API-key status | `main.rs:662-706` | TUI provider-availability report |
| Model-name mirror | `main.rs:979-1012` | TUI header model label |

Runtime provider switching uses `ProxyProvider`
(`crates/neenee/src/main.rs:29`), an `Arc<RwLock<Arc<dyn Provider>>>` holder
that hot-swaps the active provider without rebuilding the `Agent`.

## Retry

Transient HTTP `408`, `429`, `5xx`, connection, and timeout failures are
wrapped in `RetryableError` (`crates/neenee-core/src/lib.rs:14`) by
`ensure_success` (`providers.rs:35`) and `transport_error` (`providers.rs:53`).
The marker prefix is `[NEENEE_RETRYABLE]`.

Retry is a turn-level loop in `crates/neenee/src/main.rs:280-342`, not a
provider decorator. Configuration:

| Config key | Default | Hard maximum |
|------------|---------|--------------|
| `provider_retry_max_attempts` | `4` | `10` |
| `provider_retry_base_ms` | `1000` | — |
| `provider_retry_max_ms` | `30000` | — |

Backoff is exponential `base_ms * 2^(attempt-1)` capped at `max_ms`
(`main.rs:375-380`). Server `Retry-After` or `retry-after-ms` headers take
priority (`providers.rs:12-33`). Once any tool has run in the current turn,
retryable errors become terminal so tool side effects are never replayed
(`main.rs:326-328`).

## See also

- [Provider capabilities](../explanation/provider-capabilities.md) — why
  providers differ on tool and reasoning support
- [Tool protocol](../explanation/tool-protocol.md) — how the universal
  fallback covers providers without native tools
- [How to add a provider](../how-to/add-a-provider.md) — implementing a new
  adapter
- [Harness architecture](../explanation/harness.md) — provider retry and the
  harness safety bounds
