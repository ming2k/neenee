# Providers

The agent talks to LLM providers through the `Provider` trait
(`crates/neenee-core/src/lib.rs`). Every provider implementation lives in
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
| `OpenAiCompatProvider` | yes | yes | yes | `providers.rs` (`OpenAiCompatProvider`) |
| OpenAI-compatible registry presets | yes | yes | yes | `OpenAiProviderSpec` (delegates to `OpenAiCompatProvider`) |
| `GeminiProvider` | no | no | no | `providers.rs` (`GeminiProvider`) |
| `LlamaServerProvider` | no | no | no | `providers.rs` (`LlamaServerProvider`) |
| `MockProvider` | no | no | no | `providers.rs` (`MockProvider`) |

The six OpenAI-compatible presets in `OPENAI_PROVIDER_SPECS`
(`kimi-code`, `kimi`, `deepseek`, `qwen`, `glm`, `volcengine`) are built by
`OpenAiProviderSpec::build`, which returns an `OpenAiCompatProvider` with its
`id` field set to the preset identifier. They therefore inherit every
capability of `OpenAiCompatProvider`. `GeminiProvider` and `LlamaServerProvider`
are standalone adapters that never override `prepare_tools` and never send a
`tools` field; tool calls on those providers travel only through the
universal fallback.

## Provider catalog

`default_provider` in `config.toml` selects the initial provider. The same
names are accepted by `/provider switch`. API keys may be supplied through
environment variables or `config.toml` fields; model selection uses a
separate `<NAME>_MODEL` env var.

### OpenAI-compatible presets

Each row corresponds to one entry in the `OPENAI_PROVIDER_SPECS` table in
`providers.rs`. The endpoint, default model, and env vars are data in that
table, not hard-coded per struct.

| `default_provider` | Endpoint | API key env | Model env | Default / popular models |
|--------------------|----------|-------------|-----------|--------------------------|
| `kimi-code` | `https://api.kimi.com/coding/v1/chat/completions` | `KIMI_CODE_API_KEY` | `KIMI_CODE_MODEL` (ignored; model is pinned) | `kimi-for-coding` |
| `kimi` | `https://api.moonshot.cn/v1/chat/completions` | `KIMI_API_KEY` | `KIMI_MODEL` | `moonshot-v1-8k`, `moonshot-v1-32k`, `moonshot-v1-128k` |
| `deepseek` | `https://api.deepseek.com/v1/chat/completions` | `DEEPSEEK_API_KEY` | `DEEPSEEK_MODEL` | `deepseek-chat`, `deepseek-reasoner` |
| `qwen` | `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions` | `DASHSCOPE_API_KEY` | `QWEN_MODEL` | `qwen-plus`, `qwen-max`, `qwen-turbo`, `qwen-coder-plus` |
| `glm` | `https://open.bigmodel.cn/api/paas/v4/chat/completions` | `GLM_API_KEY` | `GLM_MODEL` | `glm-4-plus`, `glm-4`, `glm-4-air`, `glm-4-flash`, `glm-4v` |
| `volcengine` | `https://ark.cn-beijing.volces.com/api/v3/chat/completions` | `VOLCENGINE_API_KEY` | `VOLCENGINE_MODEL` | `deepseek-v3-250324`, `deepseek-r1-250324`, `doubao-pro-256k` |

### Bespoke providers

| `default_provider` | Struct | Endpoint | API key env | Model env | Default / popular models |
|--------------------|--------|----------|-------------|-----------|--------------------------|
| `openai` | `OpenAiCompatProvider` | `https://api.openai.com/v1/chat/completions` | `OPENAI_API_KEY` | `OPENAI_MODEL` | `gpt-4o`, `gpt-4o-mini` |
| `gemini` | `GeminiProvider` | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}` | `GEMINI_API_KEY` | `GEMINI_MODEL` | `gemini-1.5-pro`, `gemini-1.5-flash`, `gemini-2.0-flash` |
| `llama` | `LlamaServerProvider` | `${LLAMA_BASE_URL}/v1/chat/completions` | none | `LLAMA_MODEL` | user-supplied |
| `custom` | `OpenAiCompatProvider` | `${CUSTOM_BASE_URL}` | `CUSTOM_API_KEY` | `CUSTOM_MODEL` | user-supplied |
| `mock` | `MockProvider` | n/a | none | none | test fixture |

Notes:

- `kimi-code` is the only preset with a `fixed_model` (`kimi-for-coding`),
  so its `KIMI_CODE_MODEL` env var is ignored. It is also the only preset
  with a `default_user_agent`; the value defaults to `KIMI_CODE_USER_AGENT`
  (`opencode/1.17.4`) and can be overridden via the `KIMI_CODE_USER_AGENT`
  env var or `config.toml`.
- `qwen` reads its API key from `DASHSCOPE_API_KEY` but its model from
  `QWEN_MODEL`.
- `llama` and `custom` are the only providers that read a base URL; the
  registry presets hard-code their endpoint inside `OPENAI_PROVIDER_SPECS`.
- `llama` and `mock` always report as ready in the API-key status check
  (`provider_key_status` in `main.rs`); the rest require their API key env
  var to be set.

## Dispatch sites

Provider construction is centralized in `make_provider`
(`crates/neenee/src/main.rs`). It is the single source of truth shared by
startup and runtime switching:

1. If `openai_provider_spec(provider_type)` matches a registry entry,
   `OpenAiProviderSpec::build` constructs the `OpenAiCompatProvider`.
2. Otherwise a `match` handles the bespoke providers (`gemini`, `llama`,
   `custom`, `openai`); the fallthrough arm returns `MockProvider`.

| Site | Function | Purpose |
|------|----------|---------|
| Startup dispatch | `main` (initial provider block) | Reads `config.default_provider`, resolves env/config values, calls `make_provider` |
| Runtime switch | `AgentRequest::SwitchProvider` handler | Resolves a TUI-entered key/url, persists it to `config.toml`, calls `make_provider` |
| API-key status | `provider_key_status` | Reports per-provider readiness to the TUI |
| Model-name mirror | `initial_m_name` block | Friendly default model label for the TUI header |

Runtime provider switching uses `ProxyProvider` (`main.rs`), an
`Arc<RwLock<Arc<dyn Provider>>>` holder that hot-swaps the active provider
without rebuilding the `Agent`.

## Retry

Transient HTTP `408`, `429`, `5xx`, connection, and timeout failures are
wrapped in `RetryableError` (`crates/neenee-core/src/error.rs`) by
`ensure_success` and `transport_error` in `providers.rs`. The marker prefix
is `[NEENEE_RETRYABLE]`.

Retry is a turn-level loop inside `execute_turn` (`crates/neenee/src/main.rs`),
not a provider decorator. Configuration:

| Config key | Default | Hard maximum |
|------------|---------|--------------|
| `provider_retry_max_attempts` | `4` | `10` |
| `provider_retry_base_ms` | `1000` | — |
| `provider_retry_max_ms` | `30000` | — |

Backoff is computed by `retry_delay_ms` as exponential
`base_ms * 2^(attempt-1)` capped at `max_ms`. Server `Retry-After` or
`retry-after-ms` headers (parsed by `retry_after_ms` in `providers.rs`) take
priority. Once any tool has run in the current turn, retryable errors become
terminal so tool side effects are never replayed.

## See also

- [Provider capabilities](../explanation/provider-capabilities.md) — why
  providers differ on tool and reasoning support
- [Tool lifecycle](../explanation/tool-lifecycle.md) — how the universal
  fallback covers providers without native tools
- [How to add a provider](../how-to/add-a-provider.md) — implementing a new
  adapter
- [Harness architecture](../explanation/harness.md) — provider retry and the
  harness safety bounds
