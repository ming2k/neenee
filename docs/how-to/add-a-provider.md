# How to add a provider

This guide walks through wiring a new LLM provider into neenee. It assumes
the provider speaks either the OpenAI Chat Completions contract or a custom
HTTP contract. For the existing provider matrix, see
[Providers](../reference/providers.md). For the capability model that
decides which path to take, see
[Provider capabilities](../explanation/provider-capabilities.md).

All provider implementations live in
`crates/neenee-core/src/providers.rs`. Construction dispatch (turning a
provider name into a concrete provider) lives in
`crates/neenee/src/main.rs`.

## Choose a path

| Provider speaks... | Path | Effort |
|--------------------|------|--------|
| OpenAI Chat Completions (`/v1/chat/completions`, `tools`, `tool_choice`, `reasoning_content`, SSE) | Add a `OPENAI_PROVIDER_SPECS` registry entry | Tiny |
| A custom contract (different roles, no `tools` field, different streaming) | Standalone adapter | Medium |

Pick the registry path whenever possible. A registry entry inherits native
tools, reasoning, and structured streaming for free by delegating to
`OpenAiCompatProvider`.

## Add a registry entry

The `OPENAI_PROVIDER_SPECS` const table in `providers.rs` is the source of
truth for every OpenAI-compatible vendor. Each row is an `OpenAiProviderSpec`
spec; adding a provider is a single new entry instead of a delegating struct
plus trait boilerplate.

```rust
OpenAiProviderSpec {
    id: "acme",
    base_url: "https://api.acme.example/v1/chat/completions",
    default_model: "acme-1",
    env_api_key: "ACME_API_KEY",
    env_model: "ACME_MODEL",
    fixed_model: None,
    default_user_agent: None,
},
```

| Field | Purpose |
|-------|---------|
| `id` | Stable identifier used in `config.toml` (`default_provider`), `/provider switch`, and the TUI |
| `base_url` | Full chat-completions endpoint URL |
| `default_model` | Model used when neither config nor environment specifies one |
| `env_api_key` | Environment variable consulted for the API key |
| `env_model` | Environment variable consulted for a model override |
| `fixed_model` | When set, the endpoint pins this model and ignores any override (e.g. the Kimi coding endpoint) |
| `default_user_agent` | When set, the endpoint requires this user agent unless overridden |

`OpenAiProviderSpec::build` turns the spec into a concrete `OpenAiCompatProvider`,
calling `OpenAiCompatProvider::with_base_url_and_user_agent` and setting the
provider's `id` field so assistant messages are attributed correctly. The
built provider inherits `prepare_tools`, `stream_chat_events`, and the full
`chat`/`stream_chat` implementations from `OpenAiCompatProvider`.

That single table entry is the entire core change. The remaining steps wire
the new `id` into the per-provider config fields and TUI status reporting in
`main.rs`.

## Register the config fields

`crates/neenee/src/main.rs` keeps a `Config` field per provider for the
API key and model so values entered in the TUI survive a restart. Add the
new provider to these helpers, all keyed by the same `id` string:

| Helper | What to add |
|--------|-------------|
| `config_api_key` | An arm returning `config.acme_api_key.clone()` |
| `config_model` | An arm returning `config.acme_model.clone()` |
| `provider_key_status` | A row checking `ACME_API_KEY` and the config field |
| `AgentRequest::SwitchProvider` api-key persistence match | An arm assigning `config.acme_api_key = Some(key)` |
| `AgentRequest::SwitchProvider` model persistence match | An arm assigning `config.acme_model = Some(model.clone())` |

Add the matching `acme_api_key` and `acme_model` fields to the `Config`
struct in `crates/neenee/src/config.rs`, with serde rename rules matching
the existing providers.

The startup dispatch and runtime switch already route through
`make_provider`, which consults `openai_provider_spec` first. Because the
new `id` is in the registry, no new `match` arm is needed in
`make_provider` itself.

Mirror the env var names, model env var, and default model in the
[Providers](../reference/providers.md) catalog table.

### Optional: model-name mirror

If the TUI header should show a friendly default model name when no model is
configured, the registry entry already supplies it through
`spec.resolve_model(...)`, which the `initial_m_name` block consults. No
extra code is needed for registry providers.

## Build a standalone adapter

Use this path only when the provider's contract is genuinely incompatible
with OpenAI Chat Completions. `GeminiProvider` and `LlamaServerProvider` are
the existing examples.

The standalone adapter must implement at minimum `chat` and `stream_chat`.
Decide explicitly for each optional method:

| Method | If implemented | If omitted (trait default) |
|--------|----------------|---------------------------|
| `prepare_tools` | Provider declares tool schemas; native function calling works | Provider never sends `tools`; tool calls fall back to text |
| `stream_chat_events` | Provider emits `TextDelta`, `ReasoningDelta`, `ToolCallDelta` | Provider emits only `TextDelta`; reasoning and tool-call deltas are lost |

Standalone adapters that omit `prepare_tools` should hard-code
`tool_calls: None` and `reasoning_content: None` in the returned `Message`,
matching `LlamaServerProvider`. This keeps the internal contract honest: the
agent knows the provider cannot deliver those fields and routes through
`parse_tool_call` instead.

Map neenee's `Role` enum to the provider's role names in both `chat` and
`stream_chat`. The universal fallback assumes assistant text is reachable
through the standard message channel; a misnamed role breaks it.

Add a `match` arm for the new `id` in `make_provider` so the standalone
constructor is reached, and add the config/status rows described above.

## Verify

Run the test suite first:

```bash
cargo test -p neenee-core providers
cargo test -p neenee
```

Then exercise the provider end-to-end:

1. Set the API key env var and start the agent with
   `default_provider = "acme"` in `config.toml`.
2. Send a prompt that should trigger a tool call. Confirm the tool step
   renders with the right arguments and result.
3. If the model advertises reasoning support (for example an
   `acme-reasoner` variant), switch to it and confirm a thinking step
   appears.
4. Run `/provider switch acme <model>` from inside the TUI and confirm the
   header updates and the new model is used.
5. Repeat the tool-call test on a provider that uses the universal
   fallback (`gemini` or `llama`) to confirm the new provider behaves
   consistently across both transports.

## Update documentation

Update these surfaces in the same change:

- Add a row to the appropriate table in [Providers](../reference/providers.md)
  (registry preset table for OpenAI-compatible providers, bespoke table for
  standalone adapters).
- If the provider introduces a new capability shape (for example a third
  standalone adapter), update
  [Provider capabilities](../explanation/provider-capabilities.md).
- If the provider's env vars or `default_provider` key differ from the
  obvious naming, call that out explicitly.

## See also

- [Providers](../reference/providers.md) — existing provider matrix
- [Provider capabilities](../explanation/provider-capabilities.md) —
  capability layering and why providers differ
- [Tool rounds](../explanation/tool-rounds.md) — what the registry path
  inherits
