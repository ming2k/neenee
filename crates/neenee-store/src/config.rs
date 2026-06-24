use crate::fsutil;
use crate::paths;
use neenee_core::{
    CompactionPolicy, HookEventKind, McpServerConfig, SkillsConfig, WebSearchConfig,
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
/// # When true, the harness injects a hidden reminder if the model tries
/// # to end a turn with an approved plan but without calling
/// # `verify_plan_execution`. Disable for trusted fast models or
/// # plan-less workflows.
/// verify_nudge_enabled = true
/// # In-loop semantic review + anti-anchoring nudge (ADR-0030). When true the
/// # harness fires `/review` once per turn on a read-only-round streak or a
/// # repeated-call count, injecting a steering nudge on a `Stuck` verdict so a
/// # micro-adjusted re-read loop is corrected instead of running to the
/// # equality guard's hard abort. Always off on sub-agents.
/// loop_review_enabled = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Opt-in hard-stop budget: abort a turn after this many total tool
    /// rounds. `0` (the default) means uncapped. Mutated at runtime via
    /// `Agent::set_hard_stop_rounds`.
    pub hard_stop_rounds: usize,
    /// Whether the verify hard-nudge gate is active. When `true` the
    /// harness injects a hidden reminder before letting a turn end with
    /// an approved plan but no `verify_plan_execution` call. Mutated at
    /// runtime via `Agent::set_verify_nudge_enabled`.
    pub verify_nudge_enabled: bool,
    /// Whether the in-loop semantic review and anti-anchoring nudge are active
    /// (ADR-0030). When `true` the harness fires `review_now` once per turn on
    /// a read-only-round streak or a repeated-call count, injecting a steering
    /// nudge on a `Stuck` verdict. Mutated at runtime via
    /// `Agent::set_loop_review_enabled`. Always `false` on sub-agents.
    pub loop_review_enabled: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            hard_stop_rounds: 0,
            verify_nudge_enabled: true,
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

/// Transport kind for a user-defined channel Selects which
/// `Provider` implementation the catalog builds. Mirrors the built-in
/// `neenee_core::catalog::Transport` variants but stays a plain serializable
/// enum so it round-trips through TOML.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserTransport {
    #[default]
    OpenAiCompat,
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
            favorites: Vec::new(),
            providers: Vec::new(),
            skills: SkillsConfig::default(),
            websearch: WebSearchConfig::default(),
            tui: TuiConfig::default(),
            agent: AgentConfig::default(),
            hooks: Vec::new(),
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
            verify_nudge_enabled = false
        "#;
        let cfg: Config = toml::from_str(toml_full).unwrap();
        assert_eq!(cfg.agent.hard_stop_rounds, 40);
        assert!(!cfg.agent.verify_nudge_enabled);

        // Missing `[agent]` table → defaults match the documented values.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.agent.hard_stop_rounds, 0);
        assert!(cfg.agent.verify_nudge_enabled);

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
        cfg.agent.verify_nudge_enabled = false;
        let serialised = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&serialised).unwrap();
        assert_eq!(parsed.agent.hard_stop_rounds, 99);
        assert!(!parsed.agent.verify_nudge_enabled);
    }
}
