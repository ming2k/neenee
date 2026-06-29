//! Transient notice bubbles: copy result and armed-action toasts.

use neenee_tui::{
    Block as RtBlock, Borders, Color, Frame, Modifier, Paragraph, Rect, Span, {Line, Style},
};

use crate::render::Theme;
use unicode_width::UnicodeWidthStr;

pub fn draw_armed_toast(frame: &mut Frame, message: &str, theme: &Theme) {
    let size = frame.area();
    toast(frame, theme, message, theme.warn(), size.width);
}

pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.area();
    let color = if failed { theme.err() } else { theme.ok() };
    toast(frame, theme, message, color, size.width);
}

pub fn toast(frame: &mut Frame, theme: &Theme, message: &str, color: Color, width: u16) {
    let text = format!(" {} ", message.trim());
    // Inner width (text) capped, plus the two border columns.
    let inner_w = text.width() as u16;
    let toast_width = inner_w.min(58) + 2;
    let x = width.saturating_sub(toast_width).saturating_sub(2).max(1);
    let area = Rect::new(x, 1, toast_width, 3);

    let block = RtBlock::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_type(neenee_tui::BorderType::Thick)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel()));

    let line = Line::from(Span::styled(
        text,
        Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
    ));
    // Vertically center the single line within the 3-row panel.
    let para = Paragraph::new(vec![Line::from(""), line]);
    frame.render_widget(para.block(block), area);
}
