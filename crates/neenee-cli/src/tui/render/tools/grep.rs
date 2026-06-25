//! Presenters for the search/listing family: `grep`, `glob`, `list_dir`.

use super::{ResultKind, ToolPresenter, ToolView, truncate};

pub struct GrepPresenter;

impl ToolPresenter for GrepPresenter {
    fn summary(&self, view: &ToolView) -> String {
        let pattern = view.str("pattern").unwrap_or("...");
        let path = view.str("path").unwrap_or(".");
        format!("Grep \"{}\" in {}", truncate(pattern, 48), path)
    }

    fn result_kind(&self) -> ResultKind {
        ResultKind::Grep
    }
}

pub struct GlobPresenter;

impl ToolPresenter for GlobPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("pattern")
            .map(|pattern| format!("Glob {}", pattern))
            .unwrap_or_else(|| "Glob files".to_string())
    }

    fn result_kind(&self) -> ResultKind {
        ResultKind::Listing
    }
}

pub struct ListDirPresenter;

impl ToolPresenter for ListDirPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("path")
            .map(|path| format!("List {}", path))
            .unwrap_or_else(|| "List directory".to_string())
    }

    fn result_kind(&self) -> ResultKind {
        ResultKind::Listing
    }
}
