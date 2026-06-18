//! Presenter for `bash`.

use super::{truncate, ArgLayout, PreviewLine, PreviewTone, ResultKind, ToolPresenter, ToolView};
use crate::render::text_layout::strip_ansi;

/// Max output lines lifted into the collapsed preview before a `…` marker.
const BASH_PREVIEW_LINES: usize = 8;

pub struct BashPresenter;

impl ToolPresenter for BashPresenter {
    fn icon(&self) -> char {
        '❯'
    }

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

    /// Collapsed preview: a `$ command` line followed by the first
    /// [`BASH_PREVIEW_LINES`] ANSI-stripped output lines, with a `…` marker
    /// when there is more. Lets the user see what ran and roughly what it
    /// produced without expanding.
    fn collapsed_preview(&self, view: &ToolView) -> Vec<PreviewLine> {
        let Some(output) = view.output else {
            return Vec::new();
        };
        if output.trim().is_empty() {
            return Vec::new();
        }
        let command = view.str("command").unwrap_or_default();
        let command_first = command.lines().next().unwrap_or(command);

        let clean = strip_ansi(output);
        let out_lines: Vec<&str> = clean.lines().collect();
        let truncated = out_lines.len() > BASH_PREVIEW_LINES;

        let mut rows = Vec::with_capacity(2 + out_lines.len().min(BASH_PREVIEW_LINES));
        rows.push(PreviewLine {
            text: format!("$ {}", command_first),
            tone: PreviewTone::Primary,
        });
        for line in out_lines.iter().take(BASH_PREVIEW_LINES) {
            rows.push(PreviewLine {
                text: (*line).to_string(),
                tone: PreviewTone::Muted,
            });
        }
        if truncated {
            rows.push(PreviewLine {
                text: "…".to_string(),
                tone: PreviewTone::Faint,
            });
        }
        rows
    }
}
