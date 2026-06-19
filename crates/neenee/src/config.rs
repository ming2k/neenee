use crate::fsutil;
use crate::paths;
use neenee_core::mcp::McpServerConfig;
use neenee_core::skills::SkillsConfig;
use neenee_core::tools::WebSearchConfig;
use neenee_tui::config::TuiConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Transport kind for a user-defined channel (ADR-0002 phase 5). Selects which
/// `Provider` implementation the catalog builds. Mirrors the built-in
/// `neenee_core::catalog::Transport` variants but stays a plain serializable
/// enum so it round-trips through TOML.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserTransport {
    #[default]
    OpenAiCompat,
    GeminiNative,
    Llama,
    Mock,
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
pub struct UserModelConfig {
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
    pub compaction_max_chars: usize,
    pub compaction_preserve_turns: usize,
    /// Use the active model to produce an anchored, structured summary when
    /// compacting. When `false` (or when the summarization call fails) compaction
    /// falls back to the deterministic message-excerpt summary.
    pub compaction_summarize: bool,
    /// Enable cheap tool-result pruning (pre-turn and mid-turn) that clears old
    /// tool outputs in place to relieve context pressure before a full
    /// compaction is needed.
    pub compaction_prune: bool,
    /// Character budget of the most recent tool results protected from pruning.
    pub compaction_prune_protect_chars: usize,
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
    // Kimi Code subscription API
    pub kimi_code_api_key: Option<String>,
    pub kimi_code_user_agent: Option<String>,
    // DeepSeek (Flash = deepseek-chat, Pro = deepseek-reasoner); shared API key.
    pub deepseek_api_key: Option<String>,
    pub deepseek_flash_model: Option<String>,
    pub deepseek_pro_model: Option<String>,
    // Qwen (DashScope)
    pub qwen_api_key: Option<String>,
    pub qwen_model: Option<String>,
    // GLM (Zhipu)
    pub glm_api_key: Option<String>,
    pub glm_model: Option<String>,
    /// Favorite model ids for quick access in the picker (ADR-0002). Stored as
    /// a flat list of canonical ids; phase 5 migrates this into `[[models]]`
    /// entries. Backward-compatible via `#[serde(default)]`.
    #[serde(default)]
    pub favorites: Vec<String>,
    /// User-defined models that override built-ins or add new ones, each with
    /// one or more channels (ADR-0002 phase 5). Empty by default — the
    /// scattered per-provider fields above remain the source for built-ins.
    #[serde(default)]
    pub models: Vec<UserModelConfig>,
    /// Canonical default-model pointer (ADR-0002). Preferred over
    /// `default_provider` when set; `default_provider` is retained as the
    /// legacy fallback so existing configs keep working.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Skill configuration ([skills] table).
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Web tool configuration ([websearch] table): search backend, proxy, timeout.
    #[serde(default)]
    pub websearch: WebSearchConfig,
    /// TUI presentation ([tui] table): per-step-kind default expand state.
    #[serde(default)]
    pub tui: TuiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "mock".to_string(),
            mcp: HashMap::new(),
            compaction_max_chars: 120_000,
            compaction_preserve_turns: 6,
            compaction_summarize: true,
            compaction_prune: true,
            compaction_prune_protect_chars: 24_000,
            provider_retry_max_attempts: 4,
            provider_retry_base_ms: 1_000,
            provider_retry_max_ms: 30_000,
            openai_api_key: None,
            openai_model: Some("gpt-4o".to_string()),
            gemini_api_key: None,
            gemini_model: Some("gemini-2.5-flash".to_string()),
            llama_base_url: Some("http://localhost:8080".to_string()),
            llama_model: Some("local-model".to_string()),
            kimi_code_api_key: None,
            kimi_code_user_agent: Some("opencode/1.17.4".to_string()),
            deepseek_api_key: None,
            deepseek_flash_model: Some("deepseek-chat".to_string()),
            deepseek_pro_model: Some("deepseek-reasoner".to_string()),
            qwen_api_key: None,
            qwen_model: Some("qwen-plus".to_string()),
            glm_api_key: None,
            glm_model: Some("glm-4-plus".to_string()),
            favorites: Vec::new(),
            models: Vec::new(),
            default_model: None,
            skills: SkillsConfig::default(),
            websearch: WebSearchConfig::default(),
            tui: TuiConfig::default(),
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
        fsutil::atomic_write_json(&path, history)
            .map_err(|e| Box::<dyn std::error::Error>::from(e))?;
        Ok(())
    }
}
