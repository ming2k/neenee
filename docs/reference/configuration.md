# Configuration Reference

Every option in `config.toml`, with its default. The file lives at
`$XDG_CONFIG_HOME/neenee/config.toml` — see [Paths](paths.md) for the resolved
location and override precedence.

All keys are optional: a missing key, a missing table, or an absent file uses
the defaults below. Unknown keys are ignored, so removing or renaming a key
never breaks parsing.

## Compaction

Context compaction keeps the uncapped agentic loop bounded. Thresholds are
derived from the **active model's context window** (token-denominated) and
re-seeded on every provider switch, so they track the live model rather than a
fixed budget. See the [harness explanation](../explanation/agent-design/harness.md#context-projection),
the [pruning](../explanation/agent-design/context-pruning.md) and
[compaction](../explanation/agent-design/context-compaction.md) deep-dives, and
ADR-0019 / ADR-0021 for the design.

Pressure is estimated in tokens (`estimate_tokens`, ~4 chars/token) and compared
against the resolved thresholds. Each fraction is multiplied by the active
model's context window (`0` → the fallback window) to produce an absolute
threshold.

| Key | Default | Meaning |
|-----|---------|---------|
| `compaction.utilization` | `0.85` | Trigger a full summarizing compaction once pressure reaches this fraction of the window |
| `compaction.target_utilization` | `0.25` | After a full compaction, compress the model window down to this fraction |
| `compaction.prune_utilization` | `0.65` | Trigger cheap tool-result pruning at this fraction (below `utilization`) |
| `compaction.fallback_window_tokens` | `32000` | Assumed window (tokens) when the model's context window is unknown |
| `compaction_preserve_turns` | `6` | Number of recent complete user turns kept verbatim after a full compaction |
| `compaction_summarize` | `true` | Use the active model for an anchored structured summary; `false` uses the deterministic excerpt fallback |
| `compaction_prune` | `true` | Enable cheap tool-result pruning (pre-turn and mid-turn) |
| `compaction_prune_protect_tokens` | `6000` | Most recent tool results (tokens) protected from pruning |

Resolved thresholds per model (defaults):

| Model | Window | Prune at | Compact at | Target |
|-------|--------|----------|------------|--------|
| `glm-5.2`, Gemini, DeepSeek | 1,000,000 | 650,000 | 850,000 | 250,000 |
| `kimi-k2.7-code` | 262,144 | 170,393 | 222,822 | 65,536 |
| `gpt-4o` | 128,000 | 83,200 | 108,800 | 32,000 |
| unknown / local | 32,000 (fallback) | 20,800 | 27,200 | 8,000 |

```toml
[compaction]
utilization = 0.85
target_utilization = 0.25
prune_utilization = 0.65
fallback_window_tokens = 32000

compaction_preserve_turns = 6
compaction_summarize = true
compaction_prune = true
compaction_prune_protect_tokens = 6000
```

## Agent behavior

The optional `[principal]` table.

| Key | Default | Meaning |
|-----|---------|---------|
| `principal.hard_stop_rounds` | `0` | Hard-stop a turn after this many total tool rounds. `0` = uncapped (the only execution cap; compaction is the backstop) |
| `principal.loop_review_enabled` | `true` | Enables the deterministic read-loop guard's anti-anchoring nudge: when the model repeats the same read (one page or a two-page thrash) without progress, a hidden steering message is injected. Pure signature detection (no model call), non-terminating. Flipped off on envoys and the `/review` diagnostic |
| `principal.allow_model_stdin` | `false` | Whether the model may supply `stdin` bytes for a `bash` command it emits. Off by default: the bash schema exposes no `stdin` parameter and a command needing input either gets it from a human (interactive classifier → inline input panel) or fails fast with a non-interactive remedy hint (see ADR-0043). On: the bash schema dynamically adds a `stdin` field the model can fill, threaded through as a prefilled pipe — for unattended/automatic flows where no human is reachable |

```toml
[principal]
hard_stop_rounds = 0
allow_model_stdin = false
```

## Provider selection and retry

| Key | Default | Meaning |
|-----|---------|---------|
| `default_provider` | `"kimi-code"` | Provider id activated at startup and after `/provider` reset |
| `provider_retry_max_attempts` | `4` | Max retry attempts for a transient provider error within a turn |
| `provider_retry_base_ms` | `1000` | Base delay for exponential backoff, in milliseconds |
| `provider_retry_max_ms` | `30000` | Cap on the backoff delay, in milliseconds |

## Built-in provider credentials and models

API keys accept an environment variable or an inline value; see
[Providers](providers.md) for the env-var names and capability matrix.

| Key | Default model | Purpose |
|-----|---------------|---------|
| `openai_api_key`, `openai_model` | `gpt-4o` | OpenAI |
| `gemini_api_key`, `gemini_model` | `gemini-2.5-flash` | Google Gemini |
| `moonshot_api_key`, `moonshot_model` | `kimi-k2.7-code` | Moonshot / Kimi Code |
| `deepseek_api_key`, `deepseek_flash_model`, `deepseek_pro_model` | `deepseek-v4-flash` / `deepseek-v4-pro` | DeepSeek V4 (shared key) |
| `zai_api_key`, `zai_model` | `glm-5.2` | Z.AI coding plan (GLM-5) |
| `llama_base_url`, `llama_model` | `http://localhost:8080` / `local-model` | Local Llama server (keyless) |

## User-defined providers

`providers` is an array of `[[providers]]` tables, each with one or more
channels. A user entry whose `id` matches a built-in replaces it; otherwise it
adds a new model. See [Add a provider](../how-to/add-a-provider.md) for the
full schema and examples.

```toml
[[providers]]
id = "acme"
name = "Acme Relay"
default_channel = 0

  [[providers.channels]]
  label = "Default"
  transport = "openai_compat"   # openai_compat | gemini_native | llama
  model = "acme-7b"
  base_url = "https://relay.example.com/v1"
  api_key_env = "ACME_API_KEY"  # env var name; wins over api_key
```

| `favorites` | Default | Meaning |
|-----|---------|---------|
| `favorites` | `[]` | Provider ids pinned for quick access in the picker |

## Per-model reasoning settings

Anthropic reasoning knobs — `effort` (the reasoning-depth throttle) and
`thinking` (the on/off switch) — are **per model**, not per provider: an Opus
turn can run at `max` effort while a Haiku turn runs `low`. They live in the
`[model_reasoning."<model-id>"]` table, keyed by model id (ADR-0045). Both
fields are optional — an unset one defers to the model's default.

```toml
[model_reasoning."claude-opus-4-8"]
effort   = "max"     # low | medium | high | xhigh | max (clamped to the model's levels)
thinking = true      # on/off, orthogonal to effort

[model_reasoning."claude-haiku-4-5"]
effort   = "low"
thinking = false
```

This table applies wherever the named model is served — the built-in
`anthropic` provider and Anthropic-format relays alike. In the TUI, drilling
into a provider and pressing `e` on an Anthropic model opens the per-model
settings popup that edits this table (built-in models) or the channel
(user-defined models).

The legacy flat fields `anthropic_effort` / `anthropic_thinking` still work as
a provider-wide default, but a matching `[model_reasoning]` entry takes
precedence.

## Quant runtime

`neenee-quant` reads a JSON config from `NEENEE_QUANT_CONFIG`, then applies
environment overrides. Missing values keep the defaults.

### Market data

| Environment variable | Default | Meaning |
|----------------------|---------|---------|
| `NEENEE_QUANT_MARKET_DATA` | `synthetic` | Market-data adapter: `synthetic`, `synthetic-paper`, `binance`, or `binance-http` |
| `NEENEE_QUANT_BINANCE_BASE_URL` | `https://api.binance.com` | Binance-compatible HTTP base URL |

### Broker

| Environment variable | Default | Meaning |
|----------------------|---------|---------|
| `NEENEE_QUANT_BROKER` | `paper` | Broker adapter: `paper`, `paper-trading`, or `live-http` |
| `NEENEE_QUANT_LIVE_BROKER_URL` | empty | HTTPS broker gateway base URL for `live-http`. Local development may use `http://localhost:*`, `http://127.0.0.1:*`, or `http://[::1]:*` |
| `NEENEE_QUANT_LIVE_BROKER_TOKEN_ENV` | `NEENEE_QUANT_LIVE_BROKER_TOKEN` | Environment variable that contains the live broker bearer token |
| `NEENEE_QUANT_LIVE_BROKER_TOKEN` | empty | Direct live broker bearer token override |

`live-http` never enables implicitly. It fails startup unless a non-empty
token and an accepted gateway URL are present.

The live broker gateway contract is:

| Method | Path | Request | Response |
|--------|------|---------|----------|
| `GET` | `/portfolio` | Optional `symbol` query parameter | `PortfolioSnapshot` JSON |
| `POST` | `/orders` | Order request plus `client_order_id` and `quote` | `OrderDecision` JSON |
| `POST` | `/orders/{order_id}/cancel` | `order_id` and `client_cancel_id` | `OrderDecision` JSON |

`neenee-quant` fetches `/portfolio` and applies local risk checks before
posting to `/orders`. A local risk rejection does not call the gateway.

### Paper account and risk

| Environment variable | Default | Meaning |
|----------------------|---------|---------|
| `NEENEE_QUANT_PAPER_STARTING_CASH` | `100000` | Starting cash for the paper account |
| `NEENEE_QUANT_PAPER_COMMISSION_BPS` | `0` | Paper commission in basis points |
| `NEENEE_QUANT_PAPER_STATE` | empty | Optional JSON state file for persistent paper account state |
| `NEENEE_QUANT_AUDIT_LOG` | empty | Optional JSONL audit log for order decisions |
| `NEENEE_QUANT_RISK_MAX_ORDER_NOTIONAL` | `50000` | Per-order notional ceiling |
| `NEENEE_QUANT_RISK_MAX_GROSS_EXPOSURE` | `100000` | Gross exposure ceiling |
| `NEENEE_QUANT_RISK_ALLOW_SHORT_SELLING` | `false` | Whether sell orders may open short exposure |

## TUI presentation

The optional `[tui]` table. `default_expanded` maps a tool name (or `thinking`
for reasoning traces) to its default expand state.

```toml
[tui.default_expanded]
edit_file = true
bash = true
thinking = false
```

## Hooks

Lifecycle event hooks (ADR-0025): each entry runs a shell command at one
point in the agent's lifecycle. See the [hooks explanation](../explanation/agent-design/hooks.md)
for the event set, the command contract, and how hooks compose with the
permission broker and the `/pursue` stop-gate.

The `[[hooks]]` array contains one table per hook. The capability a hook has
(block / inject / observe) is implied by its `event` — see the explanation for
which event honours which.

| Key | Default | Meaning |
|-----|---------|---------|
| `hooks[].event` | — | The lifecycle event: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `PreCompact`, `PostCompact`, `Round` (ADR-0030). `Round` is `Deny`-forbidden — it may inject or observe but cannot abort the turn |
| `hooks[].matcher` | `*` | Tool-name filter. A `|`-separated list of exact names (`Write|Edit`) when only letters/digits/`_`/`|`; otherwise a regular expression. Only the tool events honour it |
| `hooks[].command` | — | Shell command run when the event matches. Receives the event JSON on stdin; replies via exit code / stdout JSON |

```toml
[[hooks]]
event   = "PostToolUse"
matcher = "Write|Edit"
command = ".neenee/hooks/lint.sh"

[[hooks]]
event   = "PreToolUse"
matcher = "Bash"
command = ".neenee/hooks/guard-rm.sh"

[[hooks]]
event   = "Stop"
command = ".neenee/hooks/ci-gate.sh"

# ADR-0030: fires once per tool round. Deny is ignored (no de-facto round cap);
# inject context or observe. Carries the read-only-round streak.
[[hooks]]
event   = "Round"
command = ".neenee/hooks/round-watch.sh"
```

## Feature tables

These sub-tables have their own reference pages; only the table name is
configured here.

| Table | Configures | Reference |
|-------|------------|-----------|
| `[skills]` | Skill sources, extra paths, disabled skills | [Skills](tools/skills.md) |
| `[websearch]` | Web-search backend, proxy, timeout | [Web tool](tools/web.md) |
| `[mcp.<server>]` | MCP servers (one table per server) | [MCP](tools/mcp.md) |
