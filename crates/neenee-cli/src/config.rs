use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub default_provider: String,
    pub openai_api_key: Option<String>,
    pub openai_model: Option<String>,
    pub gemini_api_key: Option<String>,
    pub gemini_model: Option<String>,
    pub llama_base_url: Option<String>,
    pub llama_model: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "mock".to_string(),
            openai_api_key: None,
            openai_model: Some("gpt-4o".to_string()),
            gemini_api_key: None,
            gemini_model: Some("gemini-1.5-flash".to_string()),
            llama_base_url: Some("http://localhost:8080".to_string()),
            llama_model: Some("local-model".to_string()),
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
