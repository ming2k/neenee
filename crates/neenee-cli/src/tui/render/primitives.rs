//! Tiny shared render helpers: viewport math, modal centering/backdrop, panel
//! chrome, and color arithmetic. Kept in one place so the per-component
//! modules do not need to depend on each other for these primitives.

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block as RtBlock, Clear, Paragraph},
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

/// Section rects produced by [`modal_frame`]: the header and footer are
/// `Option`al (omitted when the modal asked for none), and `body` is always
/// present and flexes to fill whatever the header/footer leave behind.
pub(super) struct ModalFrame {
    pub header: Option<Rect>,
    pub body: Rect,
    pub footer: Option<Rect>,
}

/// Paint the unified modal chrome and split the content area into sections.
///
/// Every centered modal goes through this so the panel style lives in one
/// place: a borderless solid-bg panel (no `┃` left bar) with a 2-column
/// left/right and 1-row top/bottom inner padding, then a vertical split into
/// optional `header` (1 row) / `body` (flex) / optional 1-row gap + `footer`
/// (1 row). The caller renders its own header / body / footer content into the
/// returned rects.
pub(super) fn modal_frame(
    frame: &mut Frame,
    area: Rect,
    bg: Color,
    header: bool,
    footer: bool,
) -> ModalFrame {
    frame.render_widget(Clear, area);
    frame.render_widget(RtBlock::default().style(Style::default().bg(bg)), area);
    let inner = area.inner(&Margin {
        horizontal: 2,
        vertical: 1,
    });

    // Tagged constraints so we can map split chunks back to sections:
    // 0 = header, 4 = gap after header, 1 = body, 2 = gap before footer,
    // 3 = footer. Both gaps are 1 row so the body always sits one blank line
    // below the header and one above the footer — regardless of which sections
    // a modal asks for.
    let mut tagged: Vec<(u8, Constraint)> = Vec::new();
    if header {
        tagged.push((0, Constraint::Length(1)));
        tagged.push((4, Constraint::Length(1)));
    }
    tagged.push((1, Constraint::Min(0)));
    if footer {
        tagged.push((2, Constraint::Length(1)));
        tagged.push((3, Constraint::Length(1)));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(tagged.iter().map(|(_, c)| *c))
        .split(inner);

    let mut out = ModalFrame {
        header: None,
        body: inner,
        footer: None,
    };
    for (i, (tag, _)) in tagged.iter().enumerate() {
        match tag {
            0 => out.header = Some(chunks[i]),
            1 => out.body = chunks[i],
            3 => out.footer = Some(chunks[i]),
            _ => {}
        }
    }
    out
}

/// Render a modal body with shared scroll mechanics. The `scroll` offset is
/// clamped to `[0, content_lines - visible]` (so it can never drift past the
/// last line) and, when `follow` is `Some(idx)`, nudged so row `idx` stays on
/// screen — that's how list modals keep their selection visible without a
/// separate scroll cursor. The body is rendered with `.scroll()` so anything
/// past the visible window is clipped rather than silently truncated.
pub(super) fn render_body(
    frame: &mut Frame,
    body_rect: Rect,
    lines: Vec<Line<'static>>,
    scroll: &mut usize,
    follow: Option<usize>,
) {
    let visible = body_rect.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    *scroll = (*scroll).min(max_scroll);
    if let Some(idx) = follow {
        if visible > 0 {
            if idx < *scroll {
                *scroll = idx;
            } else if idx >= *scroll + visible {
                *scroll = idx.saturating_sub(visible.saturating_sub(1));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).scroll((*scroll as u16, 0)), body_rect);
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
