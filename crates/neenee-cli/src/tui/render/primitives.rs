//! Tiny shared render helpers: viewport math, modal centering/backdrop, panel
//! chrome, and color arithmetic. Kept in one place so the per-component
//! modules do not need to depend on each other for these primitives.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Block as RtBlock,
    Frame,
};

/// Global viewport margin. Only vertical breathing room (1 cell top and
/// bottom) is reserved; horizontally every component spans the full terminal
/// width.
pub(super) const VIEWPORT_H_MARGIN: u16 = 0;
pub(super) const VIEWPORT_V_MARGIN: u16 = 1;

/// The usable area after reserving the global viewport margins (1 cell top
/// and bottom). The full `frame.size()` is only used to paint the app
/// background and the modal backdrop.
pub(super) fn viewport_rect(frame: &Frame) -> Rect {
    frame.size().inner(&ratatui::layout::Margin {
        horizontal: VIEWPORT_H_MARGIN,
        vertical: VIEWPORT_V_MARGIN,
    })
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Fake an alpha backdrop by filling an area with a dim solid color.
pub(super) fn draw_dim_backdrop(frame: &mut Frame, area: Rect, color: Color) {
    frame.render_widget(RtBlock::default().style(Style::default().bg(color)), area);
}

/// A borderless panel with a single thick colored left bar (opencode-style).
pub(super) fn panel_block(bar_color: Color, bg: Color) -> RtBlock<'static> {
    RtBlock::default()
        .borders(ratatui::widgets::Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(bar_color))
        .style(Style::default().bg(bg))
}

/// Contrast foreground for a colored background (dark text on light fills).
pub(super) fn contrast_fg(bg: Color) -> Color {
    let (r, g, b) = rgb(bg);
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance > 140.0 {
        Color::Black
    } else {
        Color::White
    }
}

pub(super) fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (224, 108, 117),
        Color::Green => (127, 216, 143),
        Color::Yellow => (229, 192, 123),
        Color::Blue => (137, 180, 250),
        Color::Magenta => (203, 166, 247),
        Color::Cyan => (86, 182, 194),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightGreen => (127, 216, 143),
        Color::LightRed => (243, 139, 168),
        _ => (128, 128, 128),
    }
}
