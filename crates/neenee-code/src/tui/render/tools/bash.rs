//! Presenter for `bash`.

use super::{ArgLayout, ResultKind, ToolPresenter, ToolView, truncate};

pub struct BashPresenter;

impl ToolPresenter for BashPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("command")
            .map(|cmd| {
                let first = cmd.lines().next().unwrap_or(cmd);
                format!("Run {}", truncate(first, 64))
            })
            .unwrap_or_else(|| "Run command".to_string())
    }

    fn result_kind(&self) -> ResultKind {
        ResultKind::Bash
    }

    fn arg_layout(&self) -> ArgLayout {
        ArgLayout::Command
    }
}
