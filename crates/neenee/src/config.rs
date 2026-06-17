use directories::ProjectDirs;
use neenee_core::{mcp::McpServerConfig, GoalChecklistItem};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub default_provider: String,
    pub harness_goal: Option<String>,
    pub harness_goal_completed: bool,
    pub harness_goal_checklist: Vec<GoalChecklistItem>,
    pub mcp: HashMap<String, McpServerConfig>,
    pub compaction_max_chars: usize,
    pub compaction_preserve_turns: usize,
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
    // Kimi Open Platform (Moonshot)
    pub kimi_api_key: Option<String>,
    pub kimi_model: Option<String>,
    // Custom OpenAI-compatible relay
    pub custom_api_key: Option<String>,
    pub custom_model: Option<String>,
    pub custom_base_url: Option<String>,
    // DeepSeek
    pub deepseek_api_key: Option<String>,
    pub deepseek_model: Option<String>,
    // Qwen (DashScope)
    pub qwen_api_key: Option<String>,
    pub qwen_model: Option<String>,
    // GLM (Zhipu)
    pub glm_api_key: Option<String>,
    pub glm_model: Option<String>,
    // Volcengine (ByteDance)
    pub volcengine_api_key: Option<String>,
    pub volcengine_model: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "mock".to_string(),
            harness_goal: None,
            harness_goal_completed: false,
            harness_goal_checklist: Vec::new(),
            mcp: HashMap::new(),
            compaction_max_chars: 120_000,
            compaction_preserve_turns: 6,
            provider_retry_max_attempts: 4,
            provider_retry_base_ms: 1_000,
            provider_retry_max_ms: 30_000,
            openai_api_key: None,
            openai_model: Some("gpt-4o".to_string()),
            gemini_api_key: None,
            gemini_model: Some("gemini-1.5-flash".to_string()),
            llama_base_url: Some("http://localhost:8080".to_string()),
            llama_model: Some("local-model".to_string()),
            kimi_code_api_key: None,
            kimi_code_user_agent: Some("opencode/1.17.4".to_string()),
            kimi_api_key: None,
            kimi_model: Some("moonshot-v1-8k".to_string()),
            custom_api_key: None,
            custom_model: Some("custom-model".to_string()),
            custom_base_url: None,
            deepseek_api_key: None,
            deepseek_model: Some("deepseek-chat".to_string()),
            qwen_api_key: None,
            qwen_model: Some("qwen-plus".to_string()),
            glm_api_key: None,
            glm_model: Some("glm-4-plus".to_string()),
            volcengine_api_key: None,
            volcengine_model: Some("deepseek-v3-250324".to_string()),
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
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(config_path, content)?;
        Ok(())
    }

    fn config_file_path() -> PathBuf {
        let proj_dirs = ProjectDirs::from("ai", "neenee", "neenee")
            .expect("Could not determine config directory");
        proj_dirs.config_dir().join("config.toml")
    }

    pub fn history_file_path() -> PathBuf {
        let proj_dirs = ProjectDirs::from("ai", "neenee", "neenee")
            .expect("Could not determine config directory");
        proj_dirs.config_dir().join("history.json")
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(history)?;
        fs::write(path, content)?;
        Ok(())
    }
}
