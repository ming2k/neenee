//! Skill metadata, frontmatter parsing, and core data types.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Where a skill came from. Higher-priority scopes override lower-priority
/// scopes when two skills share the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillScope {
    /// Bundled system skills shipped with neenee (compile-time embedded).
    System,
    /// Skills downloaded from a remote skill repository.
    Remote,
    /// User-global skills: XDG (`$XDG_DATA_HOME/neenee/skills`), external
    /// conventions (`~/.agents/skills`, `~/.claude/skills`, `~/.kimi-code/skills`),
    /// or the deprecated `~/.neenee/skills` fallback. See ADR-0013/0014.
    User,
    /// Additional paths configured in `config.toml`.
    Extra,
    /// Project-local skills (`.neenee/skills`, `.agents/skills`, etc., in the
    /// project working tree).
    Repo,
}

impl SkillScope {
    /// Priority rank: higher numbers win.
    pub fn priority(self) -> u8 {
        match self {
            SkillScope::System => 0,
            SkillScope::Remote => 1,
            SkillScope::User => 2,
            SkillScope::Extra => 3,
            SkillScope::Repo => 4,
        }
    }
}

impl fmt::Display for SkillScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillScope::System => write!(f, "system"),
            SkillScope::Remote => write!(f, "remote"),
            SkillScope::User => write!(f, "user"),
            SkillScope::Extra => write!(f, "extra"),
            SkillScope::Repo => write!(f, "repo"),
        }
    }
}

/// Policy attached to a skill, governing runtime behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPolicy {
    /// Whether the skill may be auto-loaded when the user mentions its name.
    #[serde(default = "default_true")]
    pub allow_implicit_invocation: bool,
}

impl Default for SkillPolicy {
    fn default() -> Self {
        Self {
            allow_implicit_invocation: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// A declared tool/dependency that a skill wants available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SkillDependency {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    pub description: Option<String>,
    pub transport: Option<String>,
    pub command: Option<String>,
    pub url: Option<String>,
}

/// A discovered skill with its metadata and loaded body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub scope: SkillScope,
    /// Path to the `SKILL.md` file that declares this skill.
    pub source: PathBuf,
    /// Directory that owns this skill (where relative references live).
    pub root: PathBuf,
    /// Full body of the skill (without frontmatter).
    pub content: String,
    pub policy: SkillPolicy,
    #[serde(default)]
    pub dependencies: Vec<SkillDependency>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub version: Option<String>,
    /// Whether the skill is currently enabled. Disabled skills are ignored by
    /// the catalog and implicit invocation but can still be requested by name
    /// through `use_skill` so the model can explain why nothing happened.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Skill {
    /// True when the skill may be auto-loaded on mention.
    pub fn allows_implicit_invocation(&self) -> bool {
        self.enabled && self.policy.allow_implicit_invocation
    }
}

/// Raw frontmatter extracted from a `SKILL.md` file.
#[derive(Debug, Deserialize, Default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "short-description")]
    short_description: Option<String>,
    version: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    policy: Option<SkillPolicy>,
    #[serde(default)]
    dependencies: Vec<SkillDependency>,
}

/// Parse a skill file into a [`Skill`].
///
/// `root` is the skill directory; `source` is the `SKILL.md` path inside it.
/// If the file has no YAML frontmatter, the whole file is treated as the body
/// and the name is derived from the parent directory.
pub fn parse_skill_file(
    source: &Path,
    root: &Path,
    scope: SkillScope,
    enabled: bool,
) -> Result<Skill, String> {
    let raw = std::fs::read_to_string(source)
        .map_err(|e| format!("failed to read '{}': {}", source.display(), e))?;
    parse_skill_from_str(source, root, scope, enabled, &raw)
}

/// Parse skill content already loaded into memory (e.g. from a compile-time
/// embed). Shares schema interpretation with [`parse_skill_file`] so the
/// on-disk and embedded paths can never drift.
pub fn parse_skill_from_str(
    source: &Path,
    root: &Path,
    scope: SkillScope,
    enabled: bool,
    raw: &str,
) -> Result<Skill, String> {
    let (frontmatter, body) = split_frontmatter(raw);
    let meta: SkillFrontmatter = if frontmatter.is_empty() {
        SkillFrontmatter::default()
    } else {
        serde_yaml::from_str(frontmatter)
            .map_err(|e| format!("invalid frontmatter in '{}': {}", source.display(), e))?
    };

    let name = meta
        .name
        .or_else(|| default_skill_name(source))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("skill file '{}' has no usable name", source.display()))?;

    let description = meta.description.unwrap_or_default();

    Ok(Skill {
        name,
        description,
        short_description: meta.short_description.filter(|s| !s.is_empty()),
        scope,
        source: source.to_path_buf(),
        root: root.to_path_buf(),
        content: body.trim().to_string(),
        policy: meta.policy.unwrap_or_default(),
        dependencies: meta.dependencies,
        tags: meta.tags,
        version: meta.version.filter(|s| !s.is_empty()),
        enabled,
    })
}

fn default_skill_name(source: &Path) -> Option<String> {
    source
        .parent()
        .and_then(|p| p.file_name())
        .or_else(|| source.file_stem())
        .and_then(|n| n.to_str())
        .map(|n| n.trim().to_string())
}

/// Split markdown frontmatter from body. Returns `("", body)` when there is no
/// frontmatter.
fn split_frontmatter(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return ("", trimmed);
    }
    let after_open = &trimmed[3..];
    let Some(close_idx) = after_open.find("---") else {
        return ("", trimmed);
    };
    let frontmatter = after_open[..close_idx].trim();
    let body = &after_open[close_idx + 3..];
    (frontmatter, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let dir = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: rust-expert\ndescription: Rust help\n---\n# Body\nHello",
        )
        .unwrap();

        let skill = parse_skill_file(&path, &dir, SkillScope::Repo, true).unwrap();
        assert_eq!(skill.name, "rust-expert");
        assert_eq!(skill.description, "Rust help");
        assert_eq!(skill.scope, SkillScope::Repo);
        assert!(skill.content.contains("# Body"));
        assert!(skill.policy.allow_implicit_invocation);
    }

    #[test]
    fn derives_name_from_parent_directory_without_frontmatter() {
        let dir = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        let root = dir.join("my-skill");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("SKILL.md");
        std::fs::write(&path, "# Just body").unwrap();

        let skill = parse_skill_file(&path, &root, SkillScope::User, true).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert!(skill.description.is_empty());
        assert!(skill.content.contains("Just body"));
    }

    #[test]
    fn rejects_empty_name() {
        let dir = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: ''\n---\nbody").unwrap();

        assert!(parse_skill_file(&path, &dir, SkillScope::Repo, true).is_err());
    }
}
