//! Presenters for `edit_file` and `write_file`. Both render a red/green line
//! diff in the expanded body (derived from their arguments) and a `+N -M`
//! line-count suffix in the collapsed summary.

use super::diff::line_diff_counts;
use super::{ResultKind, ToolPresenter, ToolView};

pub struct EditPresenter;

impl ToolPresenter for EditPresenter {
    fn summary(&self, view: &ToolView) -> String {
        let Some(path) = view.str("path") else {
            return "Edit file".to_string();
        };
        match (view.str("old_string"), view.str("new_string")) {
            (Some(old), Some(new)) => {
                let (added, removed) = line_diff_counts(old, new);
                format!("Edit {} +{} -{}", path, added, removed)
            }
            _ => format!("Edit {}", path),
        }
    }

    fn result_kind(&self) -> ResultKind {
        ResultKind::Diff
    }
}

pub struct WritePresenter;

impl ToolPresenter for WritePresenter {
    fn summary(&self, view: &ToolView) -> String {
        let Some(path) = view.str("path") else {
            return "Write file".to_string();
        };
        match view.str("content") {
            Some(content) => {
                let (added, _) = line_diff_counts("", content);
                format!("Write {} +{}", path, added)
            }
            None => format!("Write {}", path),
        }
    }

    fn result_kind(&self) -> ResultKind {
        ResultKind::Diff
    }
}
