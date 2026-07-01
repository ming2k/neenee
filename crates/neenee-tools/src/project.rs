//! neenee configuration initialization.
//!
//! `InitConfigTool` materializes a `.neenee/` configuration tree in a
//! directory (skills, commands, agents) and is reused by both the
//! `init_config` tool and the `/init` slash command.

use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Initialize a `.neenee/` configuration tree in a new or existing project.
pub struct InitConfigTool;

#[async_trait]
impl Tool for InitConfigTool {
    fn name(&self) -> &str {
        "init_config"
    }
    fn description(&self) -> &str {
        "Initialize a neenee configuration tree (`.neenee/` with skills, commands, and agents \
         directories, plus an AGENTS.md guide) in the given directory. Idempotent: existing files \
         are never overwritten. Use when the user wants to set up neenee for a project."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to initialize (default current dir)" }
            },
            "required": []
        })
    }
    fn scope_target(&self, arguments: &str) -> neenee_core::ScopeTarget {
        let path = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("path")?.as_str().map(str::to_string))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ".".to_string());
        neenee_core::ScopeTarget::Path(std::path::PathBuf::from(path))
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let base = args["path"].as_str().unwrap_or(".");
        let base_path = PathBuf::from(base);
        std::fs::create_dir_all(&base_path)
            .map_err(|e| format!("Failed to access '{}': {}", base, e))?;
        let created = init_neenee_config(&base_path)?;
        if created.is_empty() {
            return Ok(format!(
                "neenee is already configured in '{}'. Nothing to do.",
                base
            ));
        }
        Ok(format!(
            "Initialized neenee configuration in '{}'.\nCreated:\n{}",
            base,
            created
                .iter()
                .map(|path| format!("- {}", path))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

// --- Self-registration -----------------------------------------------------

neenee_core::register_tool!(InitConfigFactory => InitConfigTool);

/// Materialize a `.neenee/` tree. Returns the list of newly created relative
/// paths (existing files are left untouched and not reported).
pub fn init_neenee_config(base: &Path) -> Result<Vec<String>, String> {
    let mut created = Vec::new();
    let dirs = ["skills", "commands", "agents"];
    for dir in dirs {
        let path = base.join(".neenee").join(dir);
        if !path.exists() {
            std::fs::create_dir_all(&path)
                .map_err(|e| format!("Failed to create '{}': {}", path.display(), e))?;
            created.push(format!(".neenee/{}/.keep", dir));
            std::fs::write(path.join(".keep"), "")
                .map_err(|e| format!("Failed to write keep file: {}", e))?;
        }
    }

    // Drop a starter skill template so users can see the SKILL.md format.
    let example_skill = base.join(".neenee/skills/example/SKILL.md");
    if !example_skill.exists() {
        if let Some(parent) = example_skill.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {}", parent.display(), e))?;
        }
        std::fs::write(&example_skill, example_skill_template())
            .map_err(|e| format!("Failed to write '{}': {}", example_skill.display(), e))?;
        created.push(".neenee/skills/example/SKILL.md".to_string());
    }

    let agents_md = base.join("AGENTS.md");
    if !agents_md.exists() {
        std::fs::write(&agents_md, agents_md_template(base))
            .map_err(|e| format!("Failed to write AGENTS.md: {}", e))?;
        created.push("AGENTS.md".to_string());
    }

    let gitignore = base.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, neenee_gitignore())
            .map_err(|e| format!("Failed to write .gitignore: {}", e))?;
        created.push(".gitignore".to_string());
    }

    Ok(created)
}

fn example_skill_template() -> &'static str {
    "---\n\
     name: example\n\
     description: An example skill showing the frontmatter format.\n\
     short-description: Example skill\n\
     ---\n\
     \n\
     # Example Skill\n\
     \n\
     Edit this file or add more `.neenee/skills/<name>/SKILL.md` files to teach\n\
     neenee domain-specific conventions, build steps, or review checklists.\n"
}

fn agents_md_template(base: &Path) -> String {
    let project_name = base
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("this project");
    format!(
        "# {name} — Agent Guide\n\n\
         Background, architecture, and conventions coding agents need to work\n\
         effectively in this repository. Fill in the sections below as the\n\
         project matures.\n\n\
         ## Overview\n\n\
         Describe what `{name}` does and its high-level architecture.\n\n\
         ## Build & Test\n\n\
         ```\n\
         # build\n\
         # test\n\
         # lint\n\
         ```\n\n\
         ## Conventions\n\n\
         - Coding style and patterns\n\
         - Where new code should go\n\
         - Anything an agent must know before editing\n",
        name = project_name
    )
}

fn neenee_gitignore() -> &'static str {
    "# neenee\n.neenee/session.json\n.neenee/sessions/\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_neenee_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("neenee-init-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = init_neenee_config(&dir).unwrap();
        assert!(first.iter().any(|p| p == "AGENTS.md"));
        let second = init_neenee_config(&dir).unwrap();
        assert!(second.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
