# Providers

The agent talks to LLM providers through the `Provider` trait
(`crates/neenee-core/src/capability.rs`). Every provider implementation lives
in `crates/neenee-providers/src/`. Provider selection happens at startup and
on `/provider` (the picker) in `crates/neenee-code/src/main.rs`.

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
| `OpenAiCompatProvider` | yes | yes | yes | `openai_compat.rs` |
| OpenAI-compatible registry presets | yes | yes | yes | `OpenAiProviderSpec` (delegates to `OpenAiCompatProvider`) |
| `GeminiProvider` | no | no | no | `gemini.rs` |
| `LlamaServerProvider` | no | no | no | `llama.rs` |
| `MockProvider` | no | no | no | `mock.rs` |

The four OpenAI-compatible presets in `OPENAI_PROVIDER_SPECS`
(`kimi-code`, `deepseek-v4-flash`, `deepseek-v4-pro`, `zai-code`) are built by
`OpenAiProviderSpec::build`, which returns an `OpenAiCompatProvider` with its
`id` field set to the preset identifier. They therefore inherit every
capability of `OpenAiCompatProvider`. `GeminiProvider` and `LlamaServerProvider`
are standalone adapters that never override `prepare_tools` and never send a
`tools` field; tool calls on those providers travel only through the
universal fallback.

## Provider catalog

`default_provider` in `config.toml` selects the initial provider. The same
names are accepted by the `/provider` picker. API keys may be supplied through
environment variables or `config.toml` fields; model selection uses a
separate `<NAME>_MODEL` env var.

### OpenAI-compatible presets

Each row corresponds to one entry in the `OPENAI_PROVIDER_SPECS` table in
`crates/neenee-providers/src/registry.rs`. The endpoint, default model, and
env vars are data in that table, not hard-coded per struct.

| `default_provider` | Endpoint | API key env | Model env | Default / popular models |
|--------------------|----------|-------------|-----------|--------------------------|
| `kimi-code` | `https://api.kimi.com/coding/v1/chat/completions` | `MOONSHOT_API_KEY` | `MOONSHOT_MODEL` | `kimi-k2.7-code` (pinned to the latest K2.7 Code weights) |
| `deepseek-v4-flash` | `https://api.deepseek.com/v1/chat/completions` | `DEEPSEEK_API_KEY` | `DEEPSEEK_FLASH_MODEL` | `deepseek-v4-flash` |
| `deepseek-v4-pro` | `https://api.deepseek.com/v1/chat/completions` | `DEEPSEEK_API_KEY` | `DEEPSEEK_PRO_MODEL` | `deepseek-v4-pro` |
| `zai-code` | `https://api.z.ai/api/coding/paas/v4/chat/completions` | `ZAI_API_KEY` | `ZAI_MODEL` | `glm-5.2` (default), `glm-5.1`, `glm-4.7` |

### Bespoke providers

| `default_provider` | Struct | Endpoint | API key env | Model env | Default / popular models |
|--------------------|--------|----------|-------------|-----------|--------------------------|
| `openai` | `OpenAiCompatProvider` | `https://api.openai.com/v1/chat/completions` | `OPENAI_API_KEY` | `OPENAI_MODEL` | `gpt-4o`, `gpt-4o-mini` |
| `gemini` | `GeminiProvider` | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}` | `GEMINI_API_KEY` | `GEMINI_MODEL` | `gemini-2.5-flash` (default), `gemini-2.0-flash`, `gemini-1.5-pro` |
| `llama` | `LlamaServerProvider` | `${LLAMA_BASE_URL}/v1/chat/completions` | none | `LLAMA_MODEL` | user-supplied |

Notes:

- `deepseek-v4-flash` and `deepseek-v4-pro` share one API key (`DEEPSEEK_API_KEY`)
  and one endpoint; both target the V4 model family (1M context, thinking +
  non-thinking modes).
- `zai-code` targets the Z.AI (Zhipu) coding-plan platform and serves the
  GLM-5 family; it sends a `opencode/1.17.10` User-Agent so the platform
  recognises a coding agent.
- `llama` is the only provider that reads a base URL; the
  registry presets hard-code their endpoint inside `OPENAI_PROVIDER_SPECS`.
- `llama` always reports as ready in the API-key status check
  (`provider_key_status` in `main.rs`); the rest require their API key env
  var to be set.

## Dispatch sites

Provider construction is centralized in the model catalog
(`catalog::build_provider_for` / `catalog::build_catalog` in
`crates/neenee-agent/src/catalog.rs`). Every provider id — registry preset or
bespoke — is materialized into a `Channel` carrying fully resolved
credentials, model id, and transport, so startup and runtime switching share
one source of truth for the env-var-then-config resolution rules.

1. The registry presets are built from `OPENAI_PROVIDER_SPECS` via
   `OpenAiProviderSpec::build`, yielding an `OpenAiCompatProvider` with its
   `id` field set to the preset identifier.
2. The bespoke providers (`openai`, `gemini`, `llama`) get their own
   one-channel entries; an unknown id resolves to an internal `MockProvider`
   fallback (not user-visible).

| Site | Function | Purpose |
|------|----------|---------|
| Startup dispatch | `catalog::build_provider_for` | Reads `config.default_provider`, resolves env/config values via the catalog |
| Runtime switch | `AgentRequest::SwitchProvider` handler | Resolves a TUI-entered key/url, persists it to `config.toml`, rebuilds via the catalog |
| API-key status | `provider_key_status` | Reports per-provider readiness to the TUI (derived from the catalog) |
| Model-name mirror | `catalog::resolved_model_name` | Friendly default model label for the TUI header |

Runtime provider switching uses `ProxyProvider`
(`crates/neenee-agent/src/orchestration.rs`), an
`Arc<RwLock<Arc<dyn Provider>>>` holder that hot-swaps the active provider
without rebuilding the `Agent`.

## Retry

Transient HTTP `408`, `429`, `5xx`, connection, and timeout failures are
wrapped in `RetryableError` (`crates/neenee-core/src/error.rs`) by
`ensure_success` and `transport_error` in `crates/neenee-providers/src/lib.rs`.
The marker prefix
is `[NEENEE_RETRYABLE]`.

Retry is a round-level loop inside `execute_round`
(`crates/neenee-agent/src/orchestration.rs`),
not a provider decorator. Configuration:

| Config key | Default | Hard maximum |
|------------|---------|--------------|
| `provider_retry_max_attempts` | `4` | `10` |
| `provider_retry_base_ms` | `1000` | — |
| `provider_retry_max_ms` | `30000` | — |

Backoff is computed by `retry_delay_ms` as exponential
`base_ms * 2^(attempt-1)` capped at `max_ms`. Server `Retry-After` or
`retry-after-ms` headers (parsed by `retry_after_ms` in
`crates/neenee-providers/src/lib.rs`) take
priority. Once any tool has run in the current round, retryable errors become
terminal so tool side effects are never replayed.

## See also

- [Provider capabilities](../explanation/provider-capabilities.md) — why
  providers differ on tool and reasoning support
- [Tool rounds](../explanation/agent-design/rounds-and-turns.md) — how the universal
  fallback covers providers without native tools
- [How to add a provider](../how-to/add-a-provider.md) — implementing a new
  adapter
- [Harness architecture](../explanation/agent-design/harness.md) — provider retry and the
  harness safety bounds
