//! Configuration modal.

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use super::common::selectable_row;
use crate::tui::Modal;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, modal_area, modal_frame, render_body, render_modal_footer,
};

pub fn draw_config_modal(
    frame: &mut Frame,
    snapshot: &neenee_core::ConfigSnapshot,
    modal_index: usize,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::Config).expect("config modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "Config",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )])),
            h,
        );
    }

    let enabled = snapshot.progress_updates_enabled;
    let state = if enabled { "on" } else { "off" };
    let body = vec![
        selectable_row(
            0,
            modal_index,
            "Progress updates",
            state,
            true,
            if enabled { "enabled" } else { "disabled" },
            "",
            f.body.width as usize,
            theme,
        ),
        Line::from(vec![
            Span::styled("  Max chars", Style::default().fg(theme.muted())),
            Span::styled(
                format!("  {}", snapshot.progress_update_max_chars),
                Style::default().fg(theme.text),
            ),
        ]),
    ];
    render_body(frame, f.body, body, scroll, Some(modal_index), false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::primary("Space", "toggle"),
                FooterHint::always("Esc", "close"),
            ],
            theme,
        );
    }

    area
}
