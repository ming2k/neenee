//! Skill discovery across project, user, configured, remote, and system sources.

use super::metadata::{parse_skill_file, Skill, SkillScope};
use super::remote::fetch_remote_repo;
use super::SkillsConfig;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const NEENEE_SKILLS_DIR: &str = ".neenee/skills";
const SYSTEM_SKILLS_DIR: &str = ".neenee/skills/.system";
const EXTERNAL_SKILL_DIRS: &[&str] = &[".agents/skills", ".claude/skills", ".kimi-code/skills"];
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
/// skills override lower-priority skills with the same name.
pub async fn discover_all(config: &SkillsConfig) -> DiscoveryResult {
    let mut result = DiscoveryResult::default();
    let mut seen: HashSet<String> = HashSet::new();

    for source in skill_sources(config).await {
        match source {
            SkillSource::Local { root, scope } => {
                discover_local_skills(&root, scope, config, &mut seen, &mut result);
            }
            SkillSource::Remote { roots } => {
                for root in roots {
                    discover_local_skills(
                        &root,
                        SkillScope::Remote,
                        config,
                        &mut seen,
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

    // 1. Bundled/system skills (lowest priority).
    if config.bundled {
        if let Some(home) = dirs::home_dir() {
            sources.push(SkillSource::Local {
                root: home.join(SYSTEM_SKILLS_DIR),
                scope: SkillScope::System,
            });
        }
    }

    // 2. Remote skill repositories.
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

    // 3. User-global external skills.
    if let Some(home) = dirs::home_dir() {
        for dir in EXTERNAL_SKILL_DIRS {
            sources.push(SkillSource::Local {
                root: home.join(dir),
                scope: SkillScope::User,
            });
        }
        // 4. User-global neenee skills.
        sources.push(SkillSource::Local {
            root: home.join(NEENEE_SKILLS_DIR),
            scope: SkillScope::User,
        });
    }

    // 5. Configured extra paths.
    for path in &config.paths {
        let expanded = expand_tilde(path);
        sources.push(SkillSource::Local {
            root: expanded,
            scope: SkillScope::Extra,
        });
    }

    // 6. Project-local external skills.
    let project_root =
        find_project_root(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    for dir in EXTERNAL_SKILL_DIRS {
        sources.push(SkillSource::Local {
            root: project_root.join(dir),
            scope: SkillScope::Repo,
        });
    }

    // 7. Project-local neenee skills (highest priority).
    sources.push(SkillSource::Local {
        root: project_root.join(NEENEE_SKILLS_DIR),
        scope: SkillScope::Repo,
    });

    sources
}

fn discover_local_skills(
    root: &Path,
    scope: SkillScope,
    config: &SkillsConfig,
    seen: &mut HashSet<String>,
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
                    // Higher-priority source wins.
                    if seen.insert(skill.name.clone()) {
                        result.skills.push(skill);
                    }
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
}
