//! Skill discovery across project, user, configured, remote, and system sources.

use super::bundled;
use super::metadata::{parse_skill_file, Skill, SkillScope};
use super::remote::fetch_remote_repo;
use super::SkillsConfig;
use neenee_store::paths;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Project-local neenee skills directory (relative to project root).
const PROJECT_NEENEE_SKILLS_DIR: &str = ".neenee/skills";
/// External skill directory conventions (someone else's app; we read but do
/// not own these locations).
const EXTERNAL_SKILL_DIRS: &[&str] = &[".agents/skills", ".claude/skills"];
const MAX_SCAN_DEPTH: usize = 8;

/// Result of scanning every configured skill source.
#[derive(Debug, Default, Clone)]
pub struct DiscoveryResult {
    pub skills: Vec<Skill>,
    pub errors: Vec<String>,
}

/// Discover all skills using the provided configuration.
///
/// Sources are scanned from lowest to highest priority so that higher-priority
/// skills override lower-priority skills with the same name. The bundled
/// system skills (compile-time-embedded) are returned first; everything else
/// is filesystem-derived.
pub async fn discover_all(config: &SkillsConfig) -> DiscoveryResult {
    let mut result = DiscoveryResult::default();
    // name -> position in `result.skills`. Scanning runs lowest- to
    // highest-priority; `upsert_skill` makes the last claimant of a name win
    // while preserving the first-seen position for stable catalog ordering.
    let mut index: HashMap<String, usize> = HashMap::new();

    // 0. Bundled system skills (compile-time embedded; lowest priority).
    if config.bundled {
        for mut skill in bundled::discover() {
            if config.is_disabled(&skill.name) {
                skill.enabled = false;
            }
            upsert_skill(&mut result.skills, &mut index, skill);
        }
    }

    for source in skill_sources(config).await {
        match source {
            SkillSource::Local { root, scope } => {
                discover_local_skills(&root, scope, config, &mut index, &mut result);
            }
            SkillSource::Remote { roots } => {
                for root in roots {
                    discover_local_skills(
                        &root,
                        SkillScope::Remote,
                        config,
                        &mut index,
                        &mut result,
                    );
                }
            }
        }
    }

    result
}

enum SkillSource {
    Local { root: PathBuf, scope: SkillScope },
    Remote { roots: Vec<PathBuf> },
}

async fn skill_sources(config: &SkillsConfig) -> Vec<SkillSource> {
    let mut sources: Vec<SkillSource> = Vec::new();
    let dirs = paths::get();

    // 1. Remote skill repositories (priority just above bundled system).
    for url in &config.urls {
        match fetch_remote_repo(url).await {
            Ok(roots) if !roots.is_empty() => {
                sources.push(SkillSource::Remote { roots });
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("failed to fetch remote skill repo '{}': {}", url, e);
            }
        }
    }

    // 2. User-global external skill formats (someone else's app convention).
    if let Some(home) = dirs::home_dir() {
        for dir in EXTERNAL_SKILL_DIRS {
            sources.push(SkillSource::Local {
                root: home.join(dir),
                scope: SkillScope::User,
            });
        }
    }

    // 3. User-global neenee skills (XDG; the canonical user location).
    sources.push(SkillSource::Local {
        root: dirs.user_skills_dir(),
        scope: SkillScope::User,
    });

    // 4. Configured extra paths.
    for path in &config.paths {
        let expanded = expand_tilde(path);
        sources.push(SkillSource::Local {
            root: expanded,
            scope: SkillScope::Extra,
        });
    }

    // 5. Project-local external skills.
    let project_root =
        find_project_root(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    for dir in EXTERNAL_SKILL_DIRS {
        sources.push(SkillSource::Local {
            root: project_root.join(dir),
            scope: SkillScope::Repo,
        });
    }

    // 6. Project-local neenee skills (highest priority).
    sources.push(SkillSource::Local {
        root: project_root.join(PROJECT_NEENEE_SKILLS_DIR),
        scope: SkillScope::Repo,
    });

    sources
}

/// Insert a skill, or — when a higher-priority source already claimed the
/// same name — override the earlier entry in place. Scanning runs from lowest
/// to highest priority, so the last source to claim a name wins, while the
/// first-seen position is preserved for stable catalog ordering.
fn upsert_skill(skills: &mut Vec<Skill>, index: &mut HashMap<String, usize>, skill: Skill) {
    match index.get(&skill.name).copied() {
        Some(i) => skills[i] = skill,
        None => {
            index.insert(skill.name.clone(), skills.len());
            skills.push(skill);
        }
    }
}

fn discover_local_skills(
    root: &Path,
    scope: SkillScope,
    config: &SkillsConfig,
    index: &mut HashMap<String, usize>,
    result: &mut DiscoveryResult,
) {
    if !root.is_dir() {
        return;
    }

    for entry in walkdir::WalkDir::new(root)
        .max_depth(MAX_SCAN_DEPTH)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        // Skip hidden subdirectories by checking the relative path.
        if is_inside_hidden_dir(root, entry.path()) {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .map(|n| n == "SKILL.md")
            .unwrap_or(false)
        {
            let source = entry.path();
            let skill_root = source.parent().unwrap_or(root).to_path_buf();
            match parse_skill_file(source, &skill_root, scope, true) {
                Ok(mut skill) => {
                    if config.is_disabled(&skill.name) {
                        skill.enabled = false;
                    }
                    upsert_skill(&mut result.skills, index, skill);
                }
                Err(e) => result.errors.push(e),
            }
        }
    }
}

fn is_inside_hidden_dir(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative
        .ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false)
        })
}

/// Find the project root by walking upward from `start` looking for common
/// markers. Falls back to `start` if no marker is found.
fn find_project_root(start: &Path) -> PathBuf {
    const MARKERS: &[&str] = &[".neenee", ".git", "Cargo.toml", "package.json"];
    for ancestor in start.ancestors() {
        for marker in MARKERS {
            if ancestor.join(marker).exists() {
                return ancestor.to_path_buf();
            }
        }
    }
    start.to_path_buf()
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_detects_git() {
        let root = std::env::temp_dir().join(format!("neenee-root-{}", uuid::Uuid::new_v4()));
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        assert_eq!(find_project_root(&nested), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn project_root_falls_back_to_start() {
        let dir = std::env::temp_dir().join(format!("neenee-root-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_project_root(&dir), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_tilde_resolves_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/foo"), home.join("foo"));
    }

    #[test]
    fn higher_priority_source_overrides_lower_on_name_collision() {
        // Scanning order encodes priority (lowest first). A skill with the same
        // name in a later-scanned (higher-priority) source must override the
        // earlier one, while keeping the first-seen catalog position.
        let low = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        let high = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(low.join("shared")).unwrap();
        std::fs::create_dir_all(high.join("shared")).unwrap();
        std::fs::write(
            low.join("shared").join("SKILL.md"),
            "---\nname: shared\ndescription: low\n---\nlow body",
        )
        .unwrap();
        std::fs::write(
            high.join("shared").join("SKILL.md"),
            "---\nname: shared\ndescription: high\n---\nhigh body",
        )
        .unwrap();

        let config = SkillsConfig::default();
        let mut result = DiscoveryResult::default();
        let mut index: HashMap<String, usize> = HashMap::new();
        // User scope first (lower priority), then Repo (higher priority).
        discover_local_skills(&low, SkillScope::User, &config, &mut index, &mut result);
        discover_local_skills(&high, SkillScope::Repo, &config, &mut index, &mut result);

        assert_eq!(result.skills.len(), 1, "collision should not duplicate");
        let skill = &result.skills[0];
        assert_eq!(skill.scope, SkillScope::Repo, "higher-priority source wins");
        assert_eq!(skill.description, "high");
        assert_eq!(skill.content, "high body");

        let _ = std::fs::remove_dir_all(&low);
        let _ = std::fs::remove_dir_all(&high);
    }

    #[test]
    fn disabled_flag_survives_override() {
        // A higher-priority source still honours [skills] disabled for its name.
        let low = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        let high = std::env::temp_dir().join(format!("neenee-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(low.join("x")).unwrap();
        std::fs::create_dir_all(high.join("x")).unwrap();
        std::fs::write(low.join("x").join("SKILL.md"), "---\nname: x\n---\nlow").unwrap();
        std::fs::write(high.join("x").join("SKILL.md"), "---\nname: x\n---\nhigh").unwrap();

        let config = SkillsConfig {
            disabled: vec!["x".to_string()],
            ..SkillsConfig::default()
        };
        let mut result = DiscoveryResult::default();
        let mut index: HashMap<String, usize> = HashMap::new();
        discover_local_skills(&low, SkillScope::User, &config, &mut index, &mut result);
        discover_local_skills(&high, SkillScope::Repo, &config, &mut index, &mut result);

        assert_eq!(result.skills.len(), 1);
        assert!(
            !result.skills[0].enabled,
            "disabled config applies to the overriding skill"
        );

        let _ = std::fs::remove_dir_all(&low);
        let _ = std::fs::remove_dir_all(&high);
    }
}
