# How to add a provider

This guide walks through wiring a new LLM provider into neenee. It assumes
the provider speaks either the OpenAI Chat Completions contract or a custom
HTTP contract. For the existing provider matrix, see
[Providers](../reference/providers.md). For the capability model that
decides which path to take, see
[Provider capabilities](../explanation/provider-capabilities.md).

All provider implementations live in
`crates/neenee-core/src/providers.rs`. Dispatch (turning a provider name
into a constructor call) lives in `crates/neenee/src/main.rs`.

## Choose a path

| Provider speaks... | Path | Effort |
|--------------------|------|--------|
| OpenAI Chat Completions (`/v1/chat/completions`, `tools`, `tool_choice`, `reasoning_content`, SSE) | Wrap `OpenAIProvider` | Small |
| A custom contract (different roles, no `tools` field, different streaming) | Standalone adapter | Medium |

Pick the wrapper path whenever possible. The wrapper inherits native tools,
reasoning, and structured streaming for free.

## Wrap `OpenAIProvider`

Add a tuple struct around `OpenAIProvider` and a constructor that fixes the
base URL. Follow the existing wrappers (`DeepSeekProvider`,
`QwenProvider`, `GLMProvider`, `VolcengineProvider`, `KimiProvider`).

```rust
/// Acme — OpenAI-compatible endpoint.
/// Base URL: https://api.acme.example/v1/chat/completions
/// Env: `ACME_API_KEY`
/// Popular models: acme-1, acme-1-mini
pub struct AcmeProvider(OpenAIProvider);

impl AcmeProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(
            api_key,
            model,
            "https://api.acme.example/v1/chat/completions",
        ))
    }
}
```

Implement `Provider` by delegating every method to the inner
`OpenAIProvider`. The four methods are `prepare_tools`, `chat`,
`stream_chat`, and `stream_chat_events`. Copy the delegation block from
`QwenProvider` (`crates/neenee-core/src/providers.rs:844-864`) verbatim and
it will be correct.

Do **not** skip `prepare_tools` or `stream_chat_events`. Skipping
`prepare_tools` silently disables native tools; skipping
`stream_chat_events` silently disables reasoning and streaming tool-call
deltas. Both force the universal fallback even when the runtime supports
the native path.

If the provider requires a non-standard `User-Agent` (Kimi Code does), use
`OpenAIProvider::with_base_url_and_user_agent` instead and accept a
`user_agent` parameter in the constructor.

## Build a standalone adapter

Use this path only when the provider's contract is genuinely incompatible
with OpenAI Chat Completions. `GeminiProvider` (`providers.rs:397`) and
`LlamaServerProvider` (`providers.rs:565`) are the existing examples.

The standalone adapter must implement at minimum `chat` and `stream_chat`.
Decide explicitly for each optional method:

| Method | If implemented | If omitted (trait default) |
|--------|----------------|---------------------------|
| `prepare_tools` | Provider declares tool schemas; native function calling works | Provider never sends `tools`; tool calls fall back to text |
| `stream_chat_events` | Provider emits `TextDelta`, `ReasoningDelta`, `ToolCallDelta` | Provider emits only `TextDelta`; reasoning and tool-call deltas are lost |

Standalone adapters that omit `prepare_tools` should hard-code
`tool_calls: None` and `reasoning_content: None` in the returned `Message`,
matching `LlamaServerProvider` (`providers.rs:619-627`). This keeps the
internal contract honest: the agent knows the provider cannot deliver those
fields and routes through `parse_tool_call` instead.

Map neenee's `Role` enum to the provider's role names in both `chat` and
`stream_chat`. The universal fallback assumes assistant text is reachable
through the standard message channel; a misnamed role breaks it.

## Register the provider

Three sites in `crates/neenee/src/main.rs` must learn the new provider
name. All three use the same string key.

### Startup dispatch

Add an arm to `match config.default_provider.as_str()` at `main.rs:777`.
Read the API key from an env var with a `config.toml` fallback:

```rust
"acme" => {
    let api_key = std::env::var("ACME_API_KEY")
        .ok()
        .or(config.acme_api_key.clone());
    OpenAIProvider::with_base_url(
        api_key.unwrap_or_default(),
        std::env::var("ACME_MODEL")
            .ok()
            .or(config.acme_model.clone())
            .unwrap_or_else(|| "acme-1".to_string()),
        "https://api.acme.example/v1/chat/completions",
    ) as Arc<dyn Provider>
}
```

Mirror the env var name, model env var, and default model in the
[Providers](../reference/providers.md) catalog table.

### Runtime switch

Add the same arm to the second dispatch at `main.rs:1079` (inside the
`AgentRequest::SwitchProvider` handler). The model comes from the request
payload rather than an env var. Persist any TUI-entered API key by
following the pattern at `main.rs:1058-1071`.

### API-key status

Add a row to `provider_key_status` at `main.rs:662-706` so the TUI reports
whether the provider is usable. Providers without an API key (`llama`,
`mock`) return `true` unconditionally.

### Optional: model-name mirror

If the TUI header should show a friendly default model name when no model
is configured, add an arm to `main.rs:979-1012`.

## Verify

Run the test suite first:

```bash
cargo test -p neenee-core providers
cargo test -p neenee
```

Then exercise the provider end-to-end:

1. Set the API key env var and start the agent with
   `default_provider = "acme"` in `config.toml`.
2. Send a prompt that should trigger a tool call. Confirm the tool-step
   card renders with the right arguments and result.
3. If the model advertises reasoning support (for example a
   `acme-reasoner` variant), switch to it and confirm a thinking card
   appears.
4. Run `/provider switch acme <model>` from inside the TUI and confirm the
   header updates and the new model is used.
5. Repeat the tool-call test on a provider that uses the universal
   fallback (`gemini` or `llama`) to confirm the new provider behaves
   consistently across both transports.

## Update documentation

Update these surfaces in the same change:

- Add a row to both tables in [Providers](../reference/providers.md)
  (capability matrix and provider catalog).
- If the provider introduces a new capability shape (for example a second
  standalone adapter), update
  [Provider capabilities](../explanation/provider-capabilities.md).
- If the provider's env vars or `default_provider` key differ from the
  obvious naming, call that out explicitly.

## See also

- [Providers](../reference/providers.md) — existing provider matrix
- [Provider capabilities](../explanation/provider-capabilities.md) —
  capability layering and why providers differ
- [Tool protocol](../explanation/tool-protocol.md) — what the wrapper path
  inherits
