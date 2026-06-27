//! User configuration schema and persistence.
//!
//! Deserializes/serializes the TOML config file (`agent`, `tui`, providers,
//! channels, MCP servers, hooks, skills, web-search) via [`crate::fsutil`]'s
//! atomic-write helpers, and loads/saves the input history. Config is state
//! (recency-merged under a companion file lock, ADR-0018); the live
//! provider/model selection telemetry lives in [`crate::provider_usage`].

use crate::fsutil;
use crate::paths;
use neenee_core::{
    CompactionPolicy, HookEventKind, McpServerConfig, SkillsConfig, ToolDescriptionOverrides,
    WebSearchConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Reserved `[tui.default_expanded]` key that controls reasoning traces.
/// Reasoning isn't a tool, so each frontend addresses it by name.
pub const THINKING_KEY: &str = "thinking";

/// User-tunable agent behaviour, deserialized from the optional `[agent]`
/// table of `config.toml`. All fields default sensibly, so a
/// `config.toml` with no `[agent]` table (or a partially specified one)
/// is valid.
///
/// ```toml
/// [agent]
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
pub struct AgentConfig {
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
    /// `Agent::set_loop_review_enabled`, and flipped off for sub-agents and the
    /// `/review` diagnostic. Detection is pure signature bookkeeping (no model
    /// call) and the nudge is non-terminating — distinct from the removed
    /// ADR-0030 semantic review, and unrelated to on-demand `/review`.
    pub loop_review_enabled: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            hard_stop_rounds: 0,
            loop_review_enabled: true,
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
/// tool = "read_file"
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
    /// Tool name (e.g. `"bash"`, `"read_file"`, `"mcp__fs__read"`).
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
    Llama,
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
    /// Full chat-completions URL (OpenAI-compatible) or server root (Llama).
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
    // Gemini
    pub gemini_api_key: Option<String>,
    pub gemini_model: Option<String>,
    // Llama (local)
    pub llama_base_url: Option<String>,
    pub llama_model: Option<String>,
    // Moonshot / Kimi Code (membership platform). The `kimi-code` preset pins
    // its model id via the provider registry, so the model override is kept
    // only for config/schema compatibility.
    pub moonshot_api_key: Option<String>,
    pub moonshot_model: Option<String>,
    // DeepSeek V4 (Flash + Pro); shared API key.
    pub deepseek_api_key: Option<String>,
    pub deepseek_flash_model: Option<String>,
    pub deepseek_pro_model: Option<String>,
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
    /// Agent behaviour (`[agent]` table): opt-in hard-stop budget and the
    /// verify hard-nudge toggle. See [`AgentConfig`] for the per-field
    /// semantics and TOML examples.
    #[serde(default)]
    pub agent: AgentConfig,
    /// Lifecycle event hooks (`[[hooks]]` array, ADR-0025). Each entry fires a
    /// shell command at one lifecycle point; see [`HookSpec`].
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
    /// Per-model tool-description overrides (`[tool_overrides."<model-id>"]`
    /// table). When talking to the named model, each listed tool's built-in
    /// description is replaced in the function schema. See [`ToolOverridesConfig`].
    #[serde(default)]
    pub tool_overrides: ToolOverridesConfig,
}

/// Per-model tool-description overrides, deserialized from the
/// `[tool_overrides]` section of `config.toml`. Maps a model id to a set of
/// `{ tool_name = "replacement description" }` pairs; when the agent is
/// talking to that model, each listed tool's built-in description is replaced
/// in the function schema sent to the provider.
///
/// ```toml
/// # Re-word how `read_file` is pitched to kimi-k2.7-code.
/// [tool_overrides."kimi-k2.7-code"]
/// read_file = "Read a file. Pass offset/limit for large files. Never omit them."
/// todo = "Maintain the task list…"
/// ```
///
/// Tools not listed keep their built-in description; models with no entry are
/// unaffected. Only the `description` field of the function schema changes —
/// the tool name and parameters are untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolOverridesConfig(pub HashMap<String, ModelToolOverrides>);

/// One model's tool-description overrides. A transparent wrapper around the
/// `tool_name → description` map so it serializes directly as a TOML table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelToolOverrides(pub ToolDescriptionOverrides);

impl ToolOverridesConfig {
    /// Look up the description overrides for `model_id`, if any. Returns an
    /// empty map (not `None`) for unknown models so callers can always borrow
    /// `&ToolDescriptionOverrides`.
    pub fn for_model(&self, model_id: &str) -> &ToolDescriptionOverrides {
        self.0
            .get(model_id)
            .map(|m| &m.0)
            .unwrap_or_else(|| neenee_core::empty_tool_description_overrides())
    }
}

impl ModelToolOverrides {
    /// Construct from an iterator of `(tool_name, description)` pairs.
    pub fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
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
            gemini_model: Some("gemini-2.5-flash".to_string()),
            llama_base_url: Some("http://localhost:8080".to_string()),
            llama_model: Some("local-model".to_string()),
            moonshot_api_key: None,
            moonshot_model: Some("kimi-k2.7-code".to_string()),
            deepseek_api_key: None,
            deepseek_flash_model: Some("deepseek-v4-flash".to_string()),
            deepseek_pro_model: Some("deepseek-v4-pro".to_string()),
            zai_api_key: None,
            zai_model: Some("glm-5.2".to_string()),
            opencode_go_api_key: None,
            default_model: None,
            favorites: Vec::new(),
            providers: Vec::new(),
            skills: SkillsConfig::default(),
            permissions: PermissionConfig::default(),
            websearch: WebSearchConfig::default(),
            tui: TuiConfig::default(),
            agent: AgentConfig::default(),
            hooks: Vec::new(),
            tool_overrides: ToolOverridesConfig::default(),
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
        // The `[agent]` table must round-trip: partial TOML keeps defaults,
        // full TOML preserves explicit overrides. Legacy `[agent.review]`
        // sub-tables (ADR-0016) are accepted but ignored — `hard_stop_rounds`
        // now lives directly under `[agent]` (ADR-0018).
        let toml_full = r#"
            [agent]
            hard_stop_rounds = 40
        "#;
        let cfg: Config = toml::from_str(toml_full).unwrap();
        assert_eq!(cfg.agent.hard_stop_rounds, 40);

        // Missing `[agent]` table → defaults match the documented values.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.agent.hard_stop_rounds, 0);

        // A legacy `[agent.review]` block no longer maps to anything; it must
        // not break parsing (unknown sub-tables are ignored) and the new
        // direct field still round-trips.
        let toml_legacy = r#"
            [agent.review]
            review_start_round = 64
            hard_stop_rounds = 99
        "#;
        let cfg: Config = toml::from_str(toml_legacy).unwrap();
        assert_eq!(cfg.agent.hard_stop_rounds, 0);

        // Round-trip through save+load format (serialize then parse).
        let mut cfg = Config::default();
        cfg.agent.hard_stop_rounds = 99;
        let serialised = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&serialised).unwrap();
        assert_eq!(parsed.agent.hard_stop_rounds, 99);
    }

    #[test]
    fn tool_overrides_table_parses_and_resolves_per_model() {
        // The table name mirrors the Config field name (`tool_overrides`), as
        // serde maps struct fields to TOML keys verbatim — same convention as
        // `[websearch]`, `[skills]`, etc. The model id is quoted because it
        // contains dots/hyphens.
        let toml_src = r#"
            [tool_overrides."kimi-k2.7-code"]
            read_file = "Always pass offset and limit."
            todo = "Keep the list honest."

            [tool_overrides."glm-5.2"]
            bash = "Prefer explicit, idempotent commands."
        "#;
        let cfg: Config = toml::from_str(toml_src).unwrap();

        // Known model → its map; unknown tool within a known model → absent.
        let kimi = cfg.tool_overrides.for_model("kimi-k2.7-code");
        assert_eq!(kimi.get("read_file").unwrap(), "Always pass offset and limit.");
        assert_eq!(kimi.get("todo").unwrap(), "Keep the list honest.");
        assert!(kimi.get("bash").is_none());

        // A different model gets its own independent map.
        let glm = cfg.tool_overrides.for_model("glm-5.2");
        assert_eq!(glm.get("bash").unwrap(), "Prefer explicit, idempotent commands.");
        assert!(glm.get("read_file").is_none());

        // Unknown model → empty (but borrowable without an Option).
        let unknown = cfg.tool_overrides.for_model("does-not-exist");
        assert!(unknown.is_empty());

        // Absent table entirely → empty config, every lookup is empty.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.tool_overrides.for_model("kimi-k2.7-code").is_empty());
    }

    #[test]
    fn tool_overrides_round_trip_through_serialise() {
        let mut cfg = Config::default();
        let map = ModelToolOverrides::from_iter([
            ("read_file".to_string(), "desc A".to_string()),
            ("bash".to_string(), "desc B".to_string()),
        ]);
        cfg.tool_overrides.0.insert("kimi-k2.7-code".to_string(), map);
        let serialised = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&serialised).unwrap();
        let resolved = parsed.tool_overrides.for_model("kimi-k2.7-code");
        assert_eq!(resolved.get("read_file").unwrap(), "desc A");
        assert_eq!(resolved.get("bash").unwrap(), "desc B");
    }
}


