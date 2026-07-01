//! Presenters for orchestration / meta tools that act on session state rather
//! than the filesystem: `todo`, `envoy`, `use_skill`.

use super::{ToolPresenter, ToolView, truncate};

pub struct TodoPresenter;

impl ToolPresenter for TodoPresenter {
    fn summary(&self, _view: &ToolView) -> String {
        "Update todo list".to_string()
    }
}

pub struct EnvoyPresenter;

impl ToolPresenter for EnvoyPresenter {
    fn summary(&self, view: &ToolView) -> String {
        // Label by role when the envoy announced it (explore / plan /
        // verify / …); fall back to the generic "Envoy" otherwise.
        let role = view.profile.unwrap_or("Envoy");
        view.str("description")
            .map(|desc| format!("{}: {}", role, truncate(desc, 56)))
            .unwrap_or_else(|| format!("Run {} envoy", role))
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
