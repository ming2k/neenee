# How to add a provider

This guide walks through wiring a new LLM provider into neenee. For the
existing provider matrix, see [Providers](../reference/providers.md). For the
capability model that decides which path to take, see
[Provider capabilities](../explanation/provider-capabilities.md).

neenee resolves every provider through one catalog
(`build_catalog` in `crates/neenee-agent/src/catalog.rs`): it materializes
registry presets, bespoke built-ins, and user-defined entries into channels
with fully resolved credentials, then constructs the concrete `Provider` via
`build_provider_for_channel` in `crates/neenee-providers/src/registry.rs`.
Startup and `/provider switch` share this single path — there is no separate
dispatch `match` to edit for presets or user entries.

## Choose a path

| Provider speaks... | Path | Effort |
|--------------------|------|--------|
| OpenAI Chat Completions, or any endpoint reachable with a URL + key | User-defined entry in `config.toml` | None (no code) |
| OpenAI Chat Completions, and you want it shipped as a built-in | Registry entry in `OPENAI_PROVIDER_SPECS` | Small |
| A genuinely incompatible contract (different roles, no `tools` field) | Standalone adapter | Medium |

Prefer the config path for private or self-hosted endpoints, and the registry
path for a vendor preset every neenee user would want.

## Path 1: User-defined entry (no code)

Any OpenAI-compatible, Gemini-native, or Llama endpoint can be added from
`config.toml` without touching code. Add a `[[providers]]` table whose `id`
either overrides a built-in or introduces a new model:

```toml
default_provider = "acme"

[[providers]]
id = "acme"
name = "Acme"

[[providers.channels]]
label = "default"
transport = "OpenAiCompat"          # or "GeminiNative" or "Llama"
base_url = "https://api.acme.example/v1/chat/completions"
api_key_env = "ACME_API_KEY"        # env var wins over the inline key below
model = "acme-1"
```

Per-channel fields:

| Field | Meaning |
|-------|---------|
| `transport` | `OpenAiCompat`, `GeminiNative`, or `Llama` |
| `base_url` | Full chat-completions URL (OpenAI-compatible) or server root (Llama) |
| `api_key_env` | Env var name read first; empty values fall through |
| `api_key` | Inline key, used when `api_key_env` is unset or empty |
| `model` | Wire model id; falls back to the entry `id` when omitted |
| `user_agent` | OpenAI-compatible only |

An entry whose `id` matches a built-in replaces it entirely; a new `id` is
appended. One entry may carry several `channels` (e.g. a model reachable
through several relays), with `default_channel` selecting the active one. See
[ADR-0002](../adr/0002-model-channel-abstraction.md) for the channel model.

## Path 2: Registry entry (built-in OpenAI-compatible preset)

Add one row to the `OPENAI_PROVIDER_SPECS` const table in
`crates/neenee-providers/src/registry.rs`:

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
| `fixed_model` | When set, pins the model and ignores any override |
| `default_user_agent` | When set, requires this user agent unless overridden |

That single entry is the whole change for a pure-env-var preset. The catalog
loops over `OPENAI_PROVIDER_SPECS` automatically, so no `match` arm is needed.
`OpenAiProviderSpec::build` constructs the concrete `OpenAiCompatProvider`,
stamping the preset `id` so assistant messages are attributed correctly. The
preset inherits `prepare_tools`, `stream_chat_events`, and the full
`chat` / `stream_chat` implementations.

### Optional: persist the key in config

By default a registry preset reads its API key and model from the environment
variables above. To also let users persist them in `config.toml`, add the
field pair to `Config` in `crates/neenee-store/src/config.rs` and an arm to
`config_key_for` / `config_model_for` in
`crates/neenee-agent/src/catalog.rs`, keyed by the same `id`. This is a
convenience layer over env vars, not a requirement — a preset with no config
arms still works through its env vars.

## Path 3: Standalone adapter (incompatible contract)

Use this path only when the provider's contract is genuinely incompatible with
OpenAI Chat Completions. `GeminiProvider` and `LlamaServerProvider` are the
existing examples, in `crates/neenee-providers/src/`.

Implement a `Provider` struct with at minimum `chat` and `stream_chat`, and
decide explicitly for each optional method:

| Method | If implemented | If omitted (trait default) |
|--------|----------------|---------------------------|
| `prepare_tools` | Provider declares tool schemas; native function calling works | Provider never sends `tools`; tool calls fall back to text |
| `stream_chat_events` | Provider emits `TextDelta`, `ReasoningDelta`, `ToolCallDelta` | Provider emits only `TextDelta`; reasoning and tool-call deltas are lost |

Adapters that omit `prepare_tools` should return `tool_calls: None` and
`reasoning_content: None` from their messages, matching `LlamaServerProvider`.
The agent then routes tool calls through `tool_call::parse_text_tool_call`
instead of native `tool_calls`.

Then wire the adapter into the two construction sites:

1. Add a `Transport` variant in `crates/neenee-core/src/catalog.rs` and an arm
   in `build_provider_for_channel`
   (`crates/neenee-providers/src/registry.rs`) that constructs the adapter from
   the channel.
2. Materialize the entry in `build_catalog`
   (`crates/neenee-agent/src/catalog.rs`) so the catalog exposes it by `id`.

Map neenee's `Role` enum to the provider's role names in both `chat` and
`stream_chat`. The universal fallback assumes assistant text is reachable
through the standard message channel; a misnamed role breaks it.

## Verify

```bash
cargo test -p neenee-providers
cargo test -p neenee-agent catalog
```

Then exercise the provider end-to-end:

1. Set the API key env var and start the agent with
   `default_provider = "acme"` in `config.toml`.
2. Send a prompt that should trigger a tool call. Confirm the tool step
   renders with the right arguments and result.
3. If the model advertises reasoning support (e.g. an `acme-reasoner`
   variant), switch to it and confirm a thinking step appears.
4. Run `/provider switch acme <model>` from inside the TUI and confirm the
   header updates and the new model is used.
5. Repeat the tool-call test on a provider that uses the universal fallback
   (`gemini` or `llama`) to confirm the new provider behaves consistently
   across both transports.

## Update documentation

- Add a row to the appropriate table in [Providers](../reference/providers.md)
  (registry preset table for OpenAI-compatible presets, bespoke table for
  standalone adapters). User-defined entries need no doc change — they are
  config, not code.
- If the provider introduces a new capability shape (e.g. a third standalone
  adapter), update
  [Provider capabilities](../explanation/provider-capabilities.md).
- If the provider's env vars or `default_provider` key differ from the obvious
  naming, call that out explicitly.

## See also

- [Providers](../reference/providers.md) — existing provider matrix
- [Provider capabilities](../explanation/provider-capabilities.md) — capability
  layering and why providers differ
- [Request flow](../explanation/request-flow.md) — the wire contract registry
  presets inherit
- [ADR-0002](../adr/0002-model-channel-abstraction.md) — the catalog and
  channel abstraction
