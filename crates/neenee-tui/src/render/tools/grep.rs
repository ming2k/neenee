//! Presenters for the search/listing family: `grep`, `glob`, `list_dir`.

use super::{truncate, PreviewLine, PreviewTone, ResultKind, ToolPresenter, ToolView};

/// Max match lines lifted into the collapsed grep preview before a `…` marker.
const GREP_PREVIEW_LINES: usize = 3;

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

    /// Collapsed preview: the first few match lines so the user can judge the
    /// hits without expanding, with a `… +N more` tail when there are more.
    fn collapsed_preview(&self, view: &ToolView) -> Vec<PreviewLine> {
        let Some(output) = view.output else {
            return Vec::new();
        };
        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return Vec::new();
        }
        let mut rows: Vec<PreviewLine> = lines
            .iter()
            .take(GREP_PREVIEW_LINES)
            .map(|l| PreviewLine {
                text: (*l).to_string(),
                tone: PreviewTone::Muted,
            })
            .collect();
        if lines.len() > GREP_PREVIEW_LINES {
            rows.push(PreviewLine {
                text: format!("… +{} more", lines.len() - GREP_PREVIEW_LINES),
                tone: PreviewTone::Faint,
            });
        }
        rows
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
