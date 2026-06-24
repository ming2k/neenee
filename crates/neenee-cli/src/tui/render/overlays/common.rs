//! Shared helpers used across multiple overlay renderers.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::render::primitives::contrast_fg;
use crate::tui::render::Theme;
/// Compact relative time for space-constrained surfaces (e.g. the sessions
/// picker's meta column): `now` / `3m` / `2h` / `5d` / `3w` — no "ago" suffix.
pub(crate) fn relative_time_compact(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(ts);
    if diff < 60 {
        "now".to_string()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else if diff < 7 * 86_400 {
        format!("{}d", diff / 86_400)
    } else {
        format!("{}w", diff / (7 * 86_400))
    }
}

/// Truncate `s` to fit `max` display columns, appending `…` when it doesn't.

/// Width-aware so CJK/wide glyphs don't break the column budget. Used by table-
/// like modal rows to cap a long first column and leave room for the rest.
pub(crate) fn truncate_ellipsis(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0).max(1);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}


/// Display column of the caret within a rendered input field, given its char
/// index. Each modal field renders its own masked/verbatim `display` string,
/// so mapping through chars (not bytes) keeps wide glyphs and `•` masks right.
pub(crate) fn caret_column(display: &str, cursor_position: usize) -> u16 {
    let n = cursor_position.min(display.chars().count());
    let byte = display
        .char_indices()
        .nth(n)
        .map(|(i, _)| i)
        .unwrap_or(display.len());
    display[..byte].width() as u16
}

/// Draw the unified provider editor Two fields — API key
/// (masked) and model id — with `Tab` cycling focus. The composer input line
/// is borrowed for the focused field's value; `key_buf` / `model_buf` hold the
/// other field while it is unfocused. `field` is `0` for key, `1` for model id.

/// One MCP server row, unpacked for rendering. `Connected` carries the
/// per-server tool names so the MCP pane can list them rather than just a count.
pub(crate) enum McpRow {
    Connected(Vec<String>),
    Disabled,
    Failed(String),
}

impl McpRow {
    pub(crate) fn connected(tools: Vec<String>) -> Self {
        Self::Connected(tools)
    }
    pub(crate) fn disabled() -> Self {
        Self::Disabled
    }
    pub(crate) fn failed(reason: String) -> Self {
        Self::Failed(reason)
    }

    /// One-line status summary + color, shown next to the server name.
    pub(crate) fn summary(&self, theme: &Theme) -> (String, Color) {
        match self {
            Self::Connected(tools) => (format!("Connected · {} tools", tools.len()), theme.ok()),
            Self::Disabled => ("Disabled".to_string(), theme.muted()),
            Self::Failed(reason) => (format!("Failed: {}", reason), theme.err()),
        }
    }

    /// Optional second line (the tool-name list for a connected server).
    pub(crate) fn detail(&self) -> Option<String> {
        match self {
            Self::Connected(tools) if !tools.is_empty() => {
                let names: String = tools.join(", ");
                Some(format!("tools: {}", names))
            }
            _ => None,
        }
    }
}


/// Empty-list placeholder: a muted message, tuned to whether the snapshot has
/// arrived (`loaded` = true → genuinely empty; false → still loading).
pub(crate) fn placeholder(message: &str, loaded: bool, muted: Color) -> Line<'static> {
    let text = if loaded {
        message.to_string()
    } else {
        "Loading…".to_string()
    };
    Line::from(Span::styled(text, Style::default().fg(muted)))
}

/// Build a selectable list row: `▣ name  hint` with the selected row taking
/// the brand background. `state_on`/`state_off` label the enabled state shown
/// at the row's right edge; an empty `state_off` hides the badge entirely.
#[allow(clippy::too_many_arguments)]
pub(crate) fn selectable_row(
    i: usize,
    selected: usize,
    name: &str,
    hint: &str,
    enabled: bool,
    state_on: &str,
    state_off: &str,
    theme: &Theme,
) -> Line<'static> {
    let is_selected = i == selected;
    let bg = if is_selected {
        theme.brand()
    } else {
        theme.panel()
    };
    let fg = if is_selected {
        contrast_fg(theme.brand())
    } else {
        theme.fg()
    };
    let muted = if is_selected {
        contrast_fg(theme.brand())
    } else {
        theme.muted()
    };
    let mark = if enabled { "●" } else { "○" };
    let state = if enabled { state_on } else { state_off };
    let mut spans = vec![
        Span::styled(format!("{} ", mark), Style::default().bg(bg).fg(fg)),
        Span::styled(name.to_string(), Style::default().bg(bg).fg(fg)),
    ];
    if !hint.is_empty() {
        spans.push(Span::styled(
            format!("  {}", hint),
            Style::default().bg(bg).fg(muted),
        ));
    }
    if !state.is_empty() {
        spans.push(Span::styled(
            format!("  [{}]", state),
            Style::default().bg(bg).fg(muted),
        ));
    }
    Line::from(spans)
}


pub(crate) fn todo_status_glyph_color(status: neenee_core::TodoStatus, theme: &Theme, muted: Color) -> Color {
    use neenee_core::TodoStatus;
    match status {
        TodoStatus::Completed => theme.ok(),
        TodoStatus::InProgress => theme.warn(),
        TodoStatus::Pending | TodoStatus::Cancelled => muted,
    }
}

