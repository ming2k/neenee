//! Sessions picker.

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::common::{one_line, relative_time_compact, truncate_ellipsis};
use crate::modal::Modal;
use crate::render::Theme;
use crate::render::primitives::{
    FooterHint, contrast_fg, modal_area, modal_frame, render_body, render_modal_footer,
};

/// Draw the sessions picker: each row shows the session overview plus its
/// creation and last-interaction times. Enter opens the selected session.
pub fn draw_sessions_modal(
    frame: &mut Frame,
    sessions: &[neenee_core::SessionOverview],
    selected: usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::Sessions).expect("sessions modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Sessions",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let body_width = f.body.width as usize;
    let mut body: Vec<Line> = Vec::new();

    if sessions.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            "No previous sessions yet.",
            Style::default().fg(theme.muted()),
        )));
    }

    for (i, session) in sessions.iter().enumerate() {
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
        let badge = if session.active { "● " } else { "  " };
        // Drop the message count (low signal) and use compact relative times
        // (no "ago" suffix) so the meta column stays narrow and predictable.
        let meta = format!(
            "created {} · active {}",
            relative_time_compact(session.created_at),
            relative_time_compact(session.updated_at),
        );
        let meta_w = meta.width();
        // Guarantee a fixed gutter between the two columns by giving the
        // overview a width budget of `body_width - meta_w - gutter`, then
        // truncating it with an ellipsis when it overflows. That way a long
        // overview never crowds the meta column, and the gutter is constant
        // row-to-row instead of whatever slack is left over.
        const COL_GUTTER: usize = 2;
        let badge_w = badge.width();
        let col1_budget = body_width.saturating_sub(meta_w + COL_GUTTER);
        let overview = truncate_ellipsis(&one_line(&session.overview), col1_budget.saturating_sub(badge_w));
        let left = format!("{}{}", badge, overview);
        let left_w = left.width();
        let pad = body_width.saturating_sub(left_w + meta_w);
        let spans = vec![
            Span::styled(left, Style::default().bg(bg).fg(fg)),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            Span::styled(meta, Style::default().bg(bg).fg(muted)),
        ];
        body.push(Line::from(spans));
    }

    render_body(frame, f.body, body, &mut 0, Some(selected), false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::primary("Enter", "open"),
                FooterHint::secondary("d", "delete"),
                FooterHint::always("Esc", "close"),
            ],
            theme,
        );
    }
    area
}
