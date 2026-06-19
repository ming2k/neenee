//! Remote skill repository support.
//!
//! A remote skill repo is a directory tree exposed over HTTP(S) with an
//! `index.json` at its root:
//!
//! ```json
//! {
//!   "skills": [
//!     { "name": "my-skill", "files": ["SKILL.md", "reference.md"] }
//!   ]
//! }
//! ```
//!
//! The first load fetches the index and every listed file into a local cache.
//! Subsequent loads reuse the cache unless `reload_skills` is called.

use reqwest::Client;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const INDEX_FILE: &str = "index.json";
const REMOTE_SUBDIR: &str = "remote";

#[derive(Debug, Deserialize)]
pub struct RemoteSkillEntry {
    pub name: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoteSkillIndex {
    #[serde(default)]
    pub skills: Vec<RemoteSkillEntry>,
}

/// Directory where remote skill repos are cached.
pub fn remote_cache_root() -> PathBuf {
    dirs::cache_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("neenee")
        .join("skills")
        .join(REMOTE_SUBDIR)
}

/// Clear every cached remote skill repo.
pub async fn clear_remote_cache() -> Result<(), String> {
    let root = remote_cache_root();
    if root.exists() {
        tokio::fs::remove_dir_all(&root).await.map_err(|e| {
            format!(
                "failed to clear remote skill cache '{}': {}",
                root.display(),
                e
            )
        })?;
    }
    Ok(())
}

/// Fetch a remote skill repository and return the cached root directories for
/// each skill that has a `SKILL.md`.
pub async fn fetch_remote_repo(repo_url: &str) -> Result<Vec<PathBuf>, String> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("neenee/0.1 (+ai-coding-agent)")
        .build()
        .map_err(|e| format!("failed to build http client: {}", e))?;

    let base = repo_url.trim_end_matches('/');
    let index_url = format!("{}/{}", base, INDEX_FILE);
    let cache_host_dir = host_cache_dir(base);

    let index_text = client
        .get(&index_url)
        .send()
        .await
        .map_err(|e| format!("failed to fetch skill index '{}': {}", index_url, e))?
        .text()
        .await
        .map_err(|e| format!("failed to read skill index '{}': {}", index_url, e))?;

    let index: RemoteSkillIndex = serde_json::from_str(&index_text)
        .map_err(|e| format!("invalid skill index '{}': {}", index_url, e))?;

    let mut roots = Vec::new();
    for entry in index.skills {
        if !entry
            .files
            .iter()
            .any(|f| f.eq_ignore_ascii_case("SKILL.md"))
        {
            continue;
        }
        let skill_dir = cache_host_dir.join(sanitize_name(&entry.name));
        download_skill_files(&client, base, &entry, &skill_dir)
            .await
            .map_err(|e| format!("failed to download skill '{}': {}", entry.name, e))?;
        roots.push(skill_dir);
    }

    Ok(roots)
}

async fn download_skill_files(
    client: &Client,
    base: &str,
    entry: &RemoteSkillEntry,
    dest: &Path,
) -> Result<(), String> {
    tokio::fs::create_dir_all(dest)
        .await
        .map_err(|e| format!("failed to create skill cache '{}': {}", dest.display(), e))?;

    for file in &entry.files {
        let url = format!("{}/{}/{}/{}", base, entry.name, file, "");
        // Trim trailing slash added above if file had no slash.
        let url = url.trim_end_matches('/').to_string();
        let path = dest.join(file);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("failed to create '{}': {}", parent.display(), e))?;
        }
        let bytes = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("failed to fetch '{}': {}", url, e))?
            .bytes()
            .await
            .map_err(|e| format!("failed to read '{}': {}", url, e))?;
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| format!("failed to write '{}': {}", path.display(), e))?;
    }

    Ok(())
}

fn host_cache_dir(url: &str) -> PathBuf {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .replace(['/', ':', '?', '&', '='], "_");
    remote_cache_root().join(host)
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_cache_dir_sanitizes_url() {
        let dir = host_cache_dir("https://example.com/skills");
        assert!(dir.to_string_lossy().contains("example.com_skills"));
    }

    #[test]
    fn sanitize_name_replaces_special_chars() {
        assert_eq!(sanitize_name("my/skill:name"), "my_skill_name");
    }
}
