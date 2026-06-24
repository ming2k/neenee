//! Presenters for orchestration / meta tools that act on session state rather
//! than the filesystem: `todo`, `subagent`, `use_skill`, `create_project`.

use super::{truncate, ToolPresenter, ToolView};

pub struct TodoPresenter;

impl ToolPresenter for TodoPresenter {
    fn summary(&self, _view: &ToolView) -> String {
        "Update todo list".to_string()
    }
}

pub struct SubagentPresenter;

impl ToolPresenter for SubagentPresenter {
    fn summary(&self, view: &ToolView) -> String {
        // Label by role when the subagent announced it (explore / plan /
        // verify / …); fall back to the generic "Subagent" otherwise.
        let role = view.profile.unwrap_or("Subagent");
        view.str("description")
            .map(|desc| format!("{}: {}", role, truncate(desc, 56)))
            .unwrap_or_else(|| format!("Run {} subagent", role))
    }
}

pub struct UseSkillPresenter;

impl ToolPresenter for UseSkillPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("name")
            .map(|name| format!("Use skill {}", name))
            .unwrap_or_else(|| "Use skill".to_string())
    }
}

pub struct CreateProjectPresenter;

impl ToolPresenter for CreateProjectPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("name")
            .map(|name| format!("Create project {}", name))
            .unwrap_or_else(|| "Create project".to_string())
    }
}
