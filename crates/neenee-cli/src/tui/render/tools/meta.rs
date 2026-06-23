//! Presenters for orchestration / meta tools that act on session state rather
//! than the filesystem: `todo`, `task`, `use_skill`, `create_project`.

use super::{truncate, ToolPresenter, ToolView};

pub struct TodoPresenter;

impl ToolPresenter for TodoPresenter {
    fn summary(&self, _view: &ToolView) -> String {
        "Update todo list".to_string()
    }
}

pub struct TaskPresenter;

impl ToolPresenter for TaskPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("description")
            .map(|desc| format!("Task: {}", truncate(desc, 56)))
            .unwrap_or_else(|| "Run sub-task".to_string())
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
