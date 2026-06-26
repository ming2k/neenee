//! Tools for interacting with the skill registry.

use super::SkillRegistry;
use crate::Tool;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

const MAX_LISTED_FILES: usize = 10;

/// Load a skill into the conversation context.
pub struct UseSkillTool {
    pub registry: Arc<SkillRegistry>,
}

#[async_trait]
impl Tool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Load a skill into the conversation context. Skills provide domain-specific expertise. \
         Call this when the task matches a skill's description."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The skill name to load" }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let name = args["name"].as_str().ok_or("Missing 'name'")?;

        let registry = self.registry.lock();
        if let Some(skill) = registry.get(name) {
            let files = list_skill_files(&skill.root);
            Ok(format!(
                "[Skill '{}' loaded]\n{}\n[/Skill]\n\nSkill files:\n{}",
                skill.name, skill.content, files
            ))
        } else {
            Err(format!(
                "Skill '{}' not found. Available skills can be discovered via the system prompt or list_skills.",
                name
            ))
        }
    }
}

/// List all available skills.
pub struct ListSkillsTool {
    pub registry: Arc<SkillRegistry>,
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }

    fn description(&self) -> &str {
        "List all available skills with their scope, description, and enabled state."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(&self, _arguments: &str) -> Result<String, String> {
        let registry = self.registry.lock();
        Ok(super::render::format_skill_list(&registry.list()))
    }
}

/// Rescan and reload skills from disk and remote repositories.
pub struct ReloadSkillsTool {
    pub registry: Arc<SkillRegistry>,
}

#[async_trait]
impl Tool for ReloadSkillsTool {
    fn name(&self) -> &str {
        "reload_skills"
    }

    fn description(&self) -> &str {
        "Rescan local skill directories and refetch remote skill repositories. \
         Use after adding, removing, or editing skill files."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(&self, _arguments: &str) -> Result<String, String> {
        self.registry.reload().await;
        let registry = self.registry.lock();
        let count = registry.list().len();
        Ok(format!("Skills reloaded. {} skill(s) available.", count))
    }
}

fn list_skill_files(root: &std::path::Path) -> String {
    let mut files: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            files.push(rel.to_string_lossy().to_string());
        }
        if files.len() >= MAX_LISTED_FILES {
            break;
        }
    }
    if files.is_empty() {
        "(none)".to_string()
    } else {
        files.join("\n")
    }
}

// --- Self-registration -----------------------------------------------------
//
// The skill tools share one live registry, cloned out of the build context as
// `Arc<SkillRegistry>`. They decline (return `None`) when no registry was
// provided, so a context that isn't skill-aware simply gets no skill tools.

neenee_core::register_tool!(UseSkillFactory => |ctx| {
    let registry = ctx.shared::<SkillRegistry>()?;
    UseSkillTool { registry }
});
neenee_core::register_tool!(ListSkillsFactory => |ctx| {
    let registry = ctx.shared::<SkillRegistry>()?;
    ListSkillsTool { registry }
});
neenee_core::register_tool!(ReloadSkillsFactory => |ctx| {
    let registry = ctx.shared::<SkillRegistry>()?;
    ReloadSkillsTool { registry }
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;

    #[tokio::test]
    async fn use_skill_returns_not_found_for_missing_skill() {
        let registry = Arc::new(SkillRegistry::empty());
        let tool = UseSkillTool { registry };
        let result = tool.call(r#"{"name":"missing"}"#).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn list_skills_reports_empty_registry() {
        let registry = Arc::new(SkillRegistry::empty());
        let tool = ListSkillsTool { registry };
        let result = tool.call("{}").await.unwrap();
        assert!(result.contains("Available skills"));
    }
}
