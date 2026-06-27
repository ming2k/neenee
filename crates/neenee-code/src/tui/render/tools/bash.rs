//! Presenter for `bash`.

use super::{ArgLayout, ResultKind, ToolPresenter, ToolView, truncate};

pub struct BashPresenter;

impl ToolPresenter for BashPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("command")
            .map(|cmd| {
                let first = cmd.lines().next().unwrap_or(cmd);
                format!("Run {}", truncate(&strip_cd_prefix(first), 64))
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

/// Strip a leading `cd <path> && ` prefix so the collapsed summary shows the
/// meaningful command instead of the long directory-change boilerplate.
fn strip_cd_prefix(cmd: &str) -> &str {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix("cd ") {
        if let Some(pos) = rest.find(" && ") {
            return rest[pos + 4..].trim();
        }
    }
    cmd
}
