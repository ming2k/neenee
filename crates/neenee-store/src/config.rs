//! User configuration schema and persistence.
//!
//! Deserializes/serializes the TOML config file (`principal`, `tui`, providers,
//! channels, MCP servers, hooks, skills, web-search) via [`crate::fsutil`]'s
//! atomic-write helpers, and loads/saves the input history. Config is state
//! (recency-merged under a companion file lock, ADR-0018); the live
//! provider/model selection telemetry lives in [`crate::provider_usage`].

use crate::fsutil;
use crate::paths;
use neenee_core::{
    CompactionPolicy, HookEventKind, McpServerConfig, SkillsConfig, VariantSelection,
    WebSearchConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Reserved `[tui.default_expanded]` key that controls reasoning traces.
/// Reasoning isn't a tool, so each frontend addresses it by name.
pub const THINKING_KEY: &str = "thinking";

/// User-tunable principal (top-level agent) behaviour, deserialized from the optional `[principal]`
/// table of `config.toml`. All fields default sensibly, so a
/// `config.toml` with no `[principal]` table (or a partially specified one)
/// is valid.
///
/// ```toml
/// [principal]
/// # Hard-stop a turn after this many total tool rounds. 0 (the default)
/// # means no hard stop — an opt-in execution budget only. This is the sole
/// # turn cap; the loop otherwise runs until the model stops, the user
/// # interrupts, or context compaction cannot relieve pressure (ADR-0009).
/// # hard_stop_rounds = 0
/// # loop_review_enabled is accepted for backwards compatibility but is now a
/// # no-op: the automatic in-loop review and anti-anchoring nudge were removed
/// # (they could reinforce the very read-loop they targeted). On-demand
/// # `/review` remains available.
/// loop_review_enabled = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrincipalConfig {
    /// Opt-in hard-stop budget: abort a turn after this many total tool
    /// rounds. `0` (the default) means uncapped. Mutated at runtime via
    /// `Agent::set_hard_stop_rounds`.
    /// Opt-in hard-stop budget: abort a turn after this many total tool
    /// rounds. `0` (the default) means uncapped. Mutated at runtime via
    /// `Agent::set_hard_stop_rounds`.
    pub hard_stop_rounds: usize,
    /// Whether the deterministic read-loop guard may inject its anti-anchoring
    /// nudge when the model repeats the same read without progress (see
    /// `neenee_agent::loop_guard`). Default `true`; wired through
    /// `Agent::set_loop_review_enabled`, and flipped off for envoys and the
    /// `/review` diagnostic. Detection is pure signature bookkeeping (no model
    /// call) and the nudge is non-terminating — distinct from the removed
    /// ADR-0030 semantic review, and unrelated to on-demand `/review`.
    pub loop_review_enabled: bool,
    /// Whether the model may supply stdin bytes for a `bash` command it emits
    /// (the opt-in "automatic flow" path, L3.5 α). Default `false`: the bash
    /// tool schema exposes no `stdin` parameter and a command that needs input
    /// either gets it from a human (interactive-classifier → input panel) or
    /// fails fast with a non-interactive remedy hint. When `true`, the bash
    /// schema **dynamically** adds a `stdin` field the model can fill, and the
    /// dispatch layer threads it through as [`StdinPolicy::Prefilled`]. This
    /// is the explicit authorization that "input may come from the model" —
    /// without it, stdin is structurally unreachable from the model's
    /// arguments. Wired through `Agent::set_allow_model_stdin`.
    pub allow_model_stdin: bool,
}

impl Default for PrincipalConfig {
    fn default() -> Self {
        Self {
            hard_stop_rounds: 0,
            loop_review_enabled: true,
            allow_model_stdin: false,
        }
    }
}

/// User-tunable frontend presentation, deserialized from the optional `[tui]`
/// table of `config.toml`. This is the **pure-data** form shared by every
/// frontend (TUI, future GUI); frontend-specific presenter logic (e.g. the
/// TUI's per-tool default-expand lookup against its render presenters) lives
/// in the frontend crate and reads this struct as input.
///
/// All fields default sensibly, so a `config.toml` with no `[tui]` table (or
/// a partially specified one) is valid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Per-step-kind default expand state. Keys are tool names (`edit_file`,
    /// `bash`, …) or [`THINKING_KEY`] for reasoning traces.
    ///
    /// ```toml
    /// [tui.default_expanded]
    /// edit_file = true
    /// bash = true
    /// thinking = false
    /// ```
    pub default_expanded: HashMap<String, bool>,
}

/// Declarative permission configuration — the `[permissions]` table. Lets users
/// pre-declare "always allow" rules in `config.toml` so default policies are
/// data-driven, not purely interactive:
///
/// ```toml
/// [[permissions.allow]]
/// tool = "bash"
/// scope = "*"
///
/// [[permissions.allow]]
/// tool = "read_text"
/// scope = "*"
/// ```
///
/// These seed the allowlist at startup; runtime "Always" decisions still write
/// to the persisted `permissions.json`. A config rule with scope `"*"` allows
/// every call to that tool without prompting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionConfig {
    /// Rules to pre-seed the "always allow" allowlist at startup.
    pub allow: Vec<PermissionRuleConfig>,
}

/// One declarative permission rule from `[permissions]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleConfig {
    /// Tool name (e.g. `"bash"`, `"read_text"`, `"mcp__fs__read"`).
    pub tool: String,
    /// Permission scope. `"*"` matches every call to the tool; a specific scope
    /// (e.g. a path prefix) allows only matching calls.
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "*".to_string()
}

/// `Provider` implementation the catalog builds. Mirrors the built-in
/// `neenee_core::catalog::Transport` variants but stays a plain serializable
/// enum so it round-trips through TOML.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserTransport {
    #[default]
    OpenAiCompat,
    /// Anthropic-compatible `/messages` endpoint. Used by opencode-go's
    /// MiniMax/Qwen models and any Anthropic-format relay.
    Anthropic,
    GeminiNative,
}

/// One delivery channel for a user-defined model. Channels are fully
/// self-contained: each carries its own endpoint, credentials, and wire model
/// id, so a single model can expose several paths (e.g. Gemini via Google AI
/// Studio, Vertex AI, or a self-hosted relay).
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct UserChannelConfig {
    /// Display label shown in the picker (e.g. `"Vertex AI"`).
    pub label: String,
    #[serde(default)]
    pub transport: UserTransport,
    /// Environment variable name read for the API key (wins over `api_key`).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Inline API key. Used when `api_key_env` is unset or empty.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Wire model id sent in the request body. Falls back to the model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Full chat-completions URL (OpenAI-compatible) or `/messages` URL
    /// (Anthropic).
    #[serde(default)]
    pub base_url: Option<String>,
    /// `User-Agent` header (OpenAI-compatible only).
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// A user-defined model entry. When its `id` matches a built-in, the user entry
/// replaces the built-in entirely (override); otherwise it is appended as a new
/// model. A model with multiple `channels` finally enables multi-channel
/// delivery paths per ADR-0002.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct UserProviderConfig {
    /// Canonical model id. Matches a built-in id to override it.
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub channels: Vec<UserChannelConfig>,
    #[serde(default)]
    pub default_channel: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub default_provider: String,
    pub mcp: HashMap<String, McpServerConfig>,
    /// Context-compaction thresholds expressed as fractions of the active
    /// model's context window, plus a fallback window for unknown models. See
    /// [`CompactionPolicy`] for the per-field semantics.
    pub compaction: CompactionPolicy,
    pub compaction_preserve_turns: usize,
    /// Use the active model to produce an anchored, structured summary when
    /// compacting. When `false` (or when the summarization call fails) compaction
    /// falls back to the deterministic message-excerpt summary.
    pub compaction_summarize: bool,
    /// Enable cheap tool-result pruning (pre-turn and mid-turn) that clears old
    /// tool outputs in place to relieve context pressure before a full
    /// compaction is needed.
    pub compaction_prune: bool,
    /// Token budget of the most recent tool results protected from pruning.
    pub compaction_prune_protect_tokens: usize,
    pub provider_retry_max_attempts: usize,
    pub provider_retry_base_ms: u64,
    pub provider_retry_max_ms: u64,
    // OpenAI
    pub openai_api_key: Option<String>,
    pub openai_model: Option<String>,
    // Google / Gemini. The `google` provider is multi-model: the active Gemini
    // model lives in `default_model`, so there is no per-provider model slot.
    pub gemini_api_key: Option<String>,
    // Moonshot / Kimi Code (membership platform). The `kimi-code` preset pins
    // its model id via the provider registry, so the model override is kept
    // only for config/schema compatibility.
    pub moonshot_api_key: Option<String>,
    pub moonshot_model: Option<String>,
    // DeepSeek V4 (Flash + Pro); shared API key. The `deepseek` provider is
    // multi-model: the active model lives in `default_model`.
    pub deepseek_api_key: Option<String>,
    // ZAI Code (Z.AI coding-plan platform / Zhipu GLM-5 family). Shares the
    // Zhipu key with the broader ecosystem in practice, but carries its own
    // config field so the z.ai endpoint can be keyed independently.
    pub zai_api_key: Option<String>,
    pub zai_model: Option<String>,
    // OpenCode Go (opencode.ai relay). One provider hosting many models
    // (GLM/Kimi/DeepSeek/MiMo via OpenAI format, MiniMax/Qwen via Anthropic
    // /messages); a single OPENCODE_API_KEY authenticates all of them. The
    // chosen model id lives in `default_model`.
    pub opencode_go_api_key: Option<String>,
    // Anthropic `/messages` relay (the built-in `anthropic` provider). A
    // *configurable* Claude relay: `anthropic_base_url` points at the official
    // API by default but can be set to any Anthropic-compatible relay (e.g. a
    // self-hosted proxy), so users with different relay addresses need no code
    // change. One key authenticates every Claude model; the active model id
    // lives in `default_model` (multi-model provider, like opencode-go).
    pub anthropic_api_key: Option<String>,
    /// Full `/messages` endpoint URL for the `anthropic` provider. Defaults to
    /// Anthropic's official API; override with any relay's `/v1/messages` URL.
    pub anthropic_base_url: Option<String>,
    /// The model id to use within the active provider. For single-model
    /// providers this mirrors the provider's pinned model; for multi-model
    /// providers (opencode-go) it selects which of the provider's models is
    /// active. `None` falls back to the provider's default model.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Favorite provider ids for quick access in the picker. Stored as a flat
    /// list of canonical provider ids.
    #[serde(default)]
    pub favorites: Vec<String>,
    /// User-defined providers that override built-ins or add new ones, each with
    /// one or more channels Empty by default — the
    /// scattered per-provider fields above remain the source for built-ins.
    #[serde(default)]
    pub providers: Vec<UserProviderConfig>,
    /// Skill configuration (`[skills]` table).
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Declarative permission rules (`[permissions]` table). Each entry is a
    /// `[[permissions.allow]]` rule (`tool` + `scope`) pre-seeded into the
    /// allowlist at startup, so default policies are data-driven rather than
    /// only interactive. Runtime "Always" decisions still add to the persisted
    /// `permissions.json`; these config rules are re-applied on every start.
    #[serde(default)]
    pub permissions: PermissionConfig,
    /// Web tool configuration (`[websearch]` table): search backend, proxy, timeout.
    #[serde(default)]
    pub websearch: WebSearchConfig,
    /// TUI presentation (`[tui]` table): per-step-kind default expand state.
    #[serde(default)]
    pub tui: TuiConfig,
    /// Principal behaviour (`[principal]` table): opt-in hard-stop budget and the
    /// verify hard-nudge toggle. See [`PrincipalConfig`] for the per-field
    /// semantics and TOML examples.
    #[serde(default)]
    pub principal: PrincipalConfig,
    /// Lifecycle event hooks (`[[hooks]]` array, ADR-0025). Each entry fires a
    /// shell command at one lifecycle point; see [`HookSpec`].
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
    /// Per-model tool-variant selection (`[tool_variants."<model-id>"]`
    /// table). When talking to the named model, each listed capability is
    /// realized by the named variant instead of its default. See
    /// [`ToolVariantsConfig`].
    #[serde(default)]
    pub tool_variants: ToolVariantsConfig,
}

/// Per-model tool-variant selection, deserialized from the `[tool_variants]`
/// section of `config.toml`. Maps a model id to a `capability → variant_id`
/// map. A capability is realized by the named variant (a genuinely different
/// implementation/schema/description), not a re-worded copy of one impl.
///
/// ```toml
/// [tool_variants."glm-5.2"]       # model id (quoted: has dots)
/// read_text = "terse"            # capability = variant id
/// bash      = "strict"
/// ```
///
/// Capabilities and models not listed use their default variant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolVariantsConfig(pub HashMap<String, ModelToolVariants>);

/// One model's variant selection: a transparent wrapper around the
/// `capability → variant_id` map so it serializes directly as a TOML table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelToolVariants(pub VariantSelection);

impl ToolVariantsConfig {
    /// Look up the variant selection for `model_id`, if any. Returns an empty
    /// map (not `None`) for unknown models so callers can always borrow
    /// `&VariantSelection`.
    pub fn for_model(&self, model_id: &str) -> &VariantSelection {
        self.0
            .get(model_id)
            .map(|m| &m.0)
            .unwrap_or_else(|| neenee_core::empty_variant_selection())
    }
}

/// One lifecycle event hook entry (ADR-0025). Deserialized from a `[[hooks]]`
/// table in `config.toml`:
///
/// ```toml
/// [[hooks]]
/// event   = "PostToolUse"          # a [`HookEventKind`] variant
/// matcher = "Write|Edit"           # optional; tool-name `|`-list or regex
/// command = ".neenee/hooks/lint.sh"
/// ```
///
/// The command receives the [`neenee_core::HookContext`] as JSON on stdin and
/// communicates a decision via exit code / stdout JSON (see the CLI runner).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    /// When this hook fires.
    pub event: HookEventKind,
    /// Tool-name filter. `None` (or unset) matches every event; only tool
    /// events (`PreToolUse` / `PostToolUse` / `PostToolUseFailure`) honour it.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Shell command run when the event matches. Executed with the project
    /// root as cwd and the hook context as JSON on stdin.
    pub command: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "kimi-code".to_string(),
            mcp: HashMap::new(),
            compaction: CompactionPolicy::default(),
            compaction_preserve_turns: 6,
            compaction_summarize: true,
            compaction_prune: true,
            compaction_prune_protect_tokens: 6_000,
            provider_retry_max_attempts: 4,
            provider_retry_base_ms: 1_000,
            provider_retry_max_ms: 30_000,
            openai_api_key: None,
            openai_model: Some("gpt-4o".to_string()),
            gemini_api_key: None,
            moonshot_api_key: None,
            moonshot_model: Some("kimi-k2.7-code".to_string()),
            deepseek_api_key: None,
            zai_api_key: None,
            zai_model: Some("glm-5.2".to_string()),
            opencode_go_api_key: None,
            anthropic_api_key: None,
            anthropic_base_url: Some("https://api.anthropic.com/v1/messages".to_string()),
            default_model: None,
            favorites: Vec::new(),
            providers: Vec::new(),
            skills: SkillsConfig::default(),
            permissions: PermissionConfig::default(),
            websearch: WebSearchConfig::default(),
            tui: TuiConfig::default(),
            principal: PrincipalConfig::default(),
            hooks: Vec::new(),
            tool_variants: ToolVariantsConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let config_path = Self::config_file_path();
        if let Ok(content) = fs::read_to_string(config_path) {
            toml::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::config_file_path();
        let bytes = toml::to_string_pretty(self)?.into_bytes();
        fsutil::atomic_write_bytes(&config_path, &bytes)?;
        Ok(())
    }

    pub fn config_file_path() -> PathBuf {
        paths::get().config_file()
    }

    pub fn history_file_path() -> PathBuf {
        paths::get().history_file()
    }

    pub fn load_history() -> Vec<String> {
        let path = Self::history_file_path();
        if let Ok(content) = fs::read_to_string(path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    pub fn save_history(history: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::history_file_path();
        // Serialise against other `neenee` instances and merge so a concurrent
        // process's recent commands survive this write (ADR-0018). Without the
        // lock + reload the last writer would erase the other's history; the
        // merge takes the union, keeping first-seen order from disk and
        // appending this process's entries that are not already present.
        let _lock = fsutil::FileLock::acquire(&path)
            .map_err(|e| format!("could not lock history file: {e}"))?;
        let mut merged: Vec<String> = fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default();
        for entry in history {
            if !merged.iter().any(|existing| existing == entry) {
                merged.push(entry.clone());
            }
        }
        const HISTORY_CAP: usize = 1_000;
        if merged.len() > HISTORY_CAP {
            let drain = merged.len() - HISTORY_CAP;
            merged.drain(..drain);
        }
        fsutil::atomic_write_json(&path, &merged).map_err(Box::<dyn std::error::Error>::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_table_round_trips_through_toml() {
        // The `[principal]` table must round-trip: partial TOML keeps defaults,
        // full TOML preserves explicit overrides. Legacy `[agent.review]`
        // sub-tables (ADR-0016) are accepted but ignored — `hard_stop_rounds`
        // now lives directly under `[principal]` (ADR-0018).
        let toml_full = r#"
            [principal]
            hard_stop_rounds = 40
        "#;
        let cfg: Config = toml::from_str(toml_full).unwrap();
        assert_eq!(cfg.principal.hard_stop_rounds, 40);

        // Missing `[principal]` table → defaults match the documented values.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.principal.hard_stop_rounds, 0);

        // A legacy `[agent.review]` block no longer maps to anything; it must
        // not break parsing (unknown sub-tables are ignored) and the new
        // direct field still round-trips.
        let toml_legacy = r#"
            [agent.review]
            review_start_round = 64
            hard_stop_rounds = 99
        "#;
        let cfg: Config = toml::from_str(toml_legacy).unwrap();
        assert_eq!(cfg.principal.hard_stop_rounds, 0);

        // Round-trip through save+load format (serialize then parse).
        let mut cfg = Config::default();
        cfg.principal.hard_stop_rounds = 99;
        let serialised = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&serialised).unwrap();
        assert_eq!(parsed.principal.hard_stop_rounds, 99);
    }

    #[test]
    fn tool_variants_table_parses_and_resolves_per_model() {
        // The table name mirrors the Config field name (`tool_variants`), as
        // serde maps struct fields to TOML keys verbatim. The model id is
        // quoted because it contains dots/hyphens. Each entry maps a capability
        // name to the variant id chosen for that model.
        let toml_src = r#"
            [tool_variants."kimi-k2.7-code"]
            read_text = "terse"
            bash = "strict"

            [tool_variants."glm-5.2"]
            read_text = "verbose"
        "#;
        let cfg: Config = toml::from_str(toml_src).unwrap();

        // Known model → its map; unlisted capability within a known model → absent.
        let kimi = cfg.tool_variants.for_model("kimi-k2.7-code");
        assert_eq!(kimi.get("read_text").map(String::as_str), Some("terse"));
        assert_eq!(kimi.get("bash").map(String::as_str), Some("strict"));
        assert!(kimi.get("grep").is_none());

        // A different model gets its own independent map.
        let glm = cfg.tool_variants.for_model("glm-5.2");
        assert_eq!(glm.get("read_text").map(String::as_str), Some("verbose"));
        assert!(glm.get("bash").is_none());

        // Unknown model → empty (but borrowable without an Option).
        assert!(cfg.tool_variants.for_model("does-not-exist").is_empty());

        // Absent table entirely → empty config, every lookup is empty.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.tool_variants.for_model("kimi-k2.7-code").is_empty());
    }

    #[test]
    fn tool_variants_round_trip_through_serialise() {
        let mut cfg = Config::default();
        let mut sel = neenee_core::VariantSelection::new();
        sel.insert("read_text".to_string(), "terse".to_string());
        sel.insert("bash".to_string(), "strict".to_string());
        cfg.tool_variants
            .0
            .insert("kimi-k2.7-code".to_string(), ModelToolVariants(sel));
        let serialised = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&serialised).unwrap();
        let resolved = parsed.tool_variants.for_model("kimi-k2.7-code");
        assert_eq!(resolved.get("read_text").map(String::as_str), Some("terse"));
        assert_eq!(resolved.get("bash").map(String::as_str), Some("strict"));
    }
}
