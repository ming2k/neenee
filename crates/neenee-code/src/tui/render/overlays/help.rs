//! Help / keybindings modal.

use neenee_tui::{
    Frame, Modifier, Paragraph, Span, {Line, Style},
};

use crate::tui::Modal;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, modal_area, modal_frame, render_body, render_modal_footer,
};

pub fn draw_help_modal(frame: &mut Frame, scroll: &mut usize, theme: &Theme) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::Help).expect("help modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Help",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let key = |k: &str| {
        Span::styled(
            format!("{:<10}", k),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &str| Span::styled(d.to_string(), Style::default().fg(theme.muted()));
    let section = |title: &str| {
        Span::styled(
            title.to_string(),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        )
    };
    let row = |k: &str, d: &str| Line::from(vec![key(k), desc(d)]);

    let body = vec![
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("alt+enter", "insert newline (ctrl+j)"),
        row("esc", "interrupt (×2) / close"),
        row("ctrl+c", "copy · clear input · quit (×2)"),
        Line::from(""),
        Line::from(section("Line editing")),
        row("ctrl+a / ctrl+e", "caret to line start / end"),
        row("ctrl+b", "move back one char (←)"),
        row("home / end", "caret to line start / end"),
        row("ctrl+u / ctrl+k", "delete to line start / end"),
        row("ctrl+w", "delete previous word"),
        row("alt+backspace", "delete previous word"),
        row("alt+d", "delete next word"),
        row("ctrl+← / ctrl+→", "move word back / forward"),
        row("alt+b / alt+f", "move word back / forward"),
        Line::from(""),
        Line::from(section("Transcript focus")),
        Line::from(desc(
            "No modes: typing always lands in the prompt. Ctrl+↑/↓ highlights",
        )),
        Line::from(desc(
            "a step; the highlight tells you which keys act on it.",
        )),
        row("ctrl+↑ / ctrl+↓", "focus a step (nearest first)"),
        row("↑ / ↓", "while focused: cycle steps"),
        row("enter", "open the focused step"),
        row("esc", "clear the focus"),
        Line::from(""),
        Line::from(section("Views & tools")),
        row("ctrl+h", "this help"),
        row("/config", "configuration"),
        row("/session", "session context"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("/", "slash commands"),
        Line::from(""),
        Line::from(section("Modes")),
        row("/pursue", "pursue a pursuit until it is met"),
        Line::from(""),
        Line::from(desc("Drag to select · Ctrl+C or Ctrl+Shift+C to copy.")),
    ];
    render_body(frame, f.body, body, scroll, None, true, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::navigation("↑↓", "scroll"),
                FooterHint::always("Esc", "close"),
            ],
            theme,
        );
    }
    area
}
