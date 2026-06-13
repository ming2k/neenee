//! Skills system: discover, load, and inject domain-specific expertise.
//!
//! Skills are markdown files with YAML frontmatter, stored in:
//!   - Project-local: `.neenee/skills/` (highest priority)
//!   - User-global: `~/.neenee/skills/` (fallback)
//!
//! Frontmatter schema:
//!   ```yaml
//!   ---
//!   name: rust-expert
//!   description: "Use when writing or debugging Rust code"
//!   ---
//!   ```
//!
//! At startup, only frontmatter (metadata) is scanned to build a compact index.
//! The full content is loaded on-demand via the `use_skill` tool.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A discovered skill with its metadata and optionally loaded content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub source: PathBuf,
    /// Full content (without frontmatter), loaded on demand.
    pub content: String,
}

/// Frontmatter extracted from a skill markdown file.
#[derive(Debug, Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Discover skills from standard directories.
pub fn discover_skills() -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen_names = HashSet::new();

    // Priority: project-local first, then user-global
    let dirs = vec![project_skills_dir(), user_skills_dir()];

    for dir in dirs {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                if let Some(skill) = parse_skill_file(&path) {
                    // Higher priority wins (project-local overrides user-global)
                    if !seen_names.contains(&skill.name) {
                        seen_names.insert(skill.name.clone());
                        skills.push(skill);
                    }
                }
            }
        }
    }

    skills
}

/// Parse a skill markdown file: extract frontmatter + body.
fn parse_skill_file(path: &Path) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&raw)?;

    let meta: Frontmatter = serde_yaml::from_str(frontmatter).unwrap_or_default();
    let name = meta.name.unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    Some(Skill {
        name,
        description: meta.description,
        source: path.to_path_buf(),
        content: body.trim().to_string(),
    })
}

/// Split markdown frontmatter from body.
/// Expects `---` delimiters at the start of the file.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        // No frontmatter — treat entire file as body
        return Some(("", trimmed));
    }
    let after_open = &trimmed[3..];
    let close_idx = after_open.find("---")?;
    let frontmatter = after_open[..close_idx].trim();
    let body = &after_open[close_idx + 3..];
    Some((frontmatter, body))
}

/// Build a compact skills index for the system prompt.
/// Only includes name + description — content is loaded on demand.
pub fn build_skills_index(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return "No skills discovered.\n".to_string();
    }
    let mut lines = vec!["Available skills (call use_skill to load full content):".to_string()];
    for skill in skills {
        let desc = skill.description.as_deref().unwrap_or("No description");
        lines.push(format!("  - {}: {}", skill.name, desc));
    }
    lines.join("\n")
}

/// Get the user-global skills directory.
fn user_skills_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".neenee")
        .join("skills")
}

/// Get the project-local skills directory.
fn project_skills_dir() -> PathBuf {
    PathBuf::from(".neenee/skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_frontmatter() {
        let text = "---\nname: test\n---\n# Body\nHello";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert!(fm.contains("name: test"));
        assert!(body.contains("# Body"));
    }

    #[test]
    fn test_split_no_frontmatter() {
        let text = "# Just markdown\nNo frontmatter";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "");
        assert!(body.contains("Just markdown"));
    }
}
