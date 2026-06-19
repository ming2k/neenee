//! Presenter for `read_file`.

use super::{ToolPresenter, ToolView};

pub struct ReadPresenter;

impl ToolPresenter for ReadPresenter {
    fn summary(&self, view: &ToolView) -> String {
        view.str("path")
            .map(|path| format!("Read {}", path))
            .unwrap_or_else(|| "Read file".to_string())
    }
}
