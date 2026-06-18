//! Presenters for `webfetch` and `websearch`.

use super::{truncate, ToolPresenter, ToolView};

pub struct WebFetchPresenter;

impl ToolPresenter for WebFetchPresenter {
    fn icon(&self) -> char {
        '⊕'
    }

    fn summary(&self, view: &ToolView) -> String {
        view.str("url")
            .map(|url| format!("Fetch {}", url))
            .unwrap_or_else(|| "Fetch URL".to_string())
    }
}

pub struct WebSearchPresenter;

impl ToolPresenter for WebSearchPresenter {
    fn icon(&self) -> char {
        '⌕'
    }

    fn summary(&self, view: &ToolView) -> String {
        view.str("query")
            .map(|query| format!("Search \"{}\"", truncate(query, 56)))
            .unwrap_or_else(|| "Web search".to_string())
    }
}
