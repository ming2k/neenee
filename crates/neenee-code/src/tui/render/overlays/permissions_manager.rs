//! Permissions manager modal — the "always allow" rule management surface.
//!
//! Distinct from [`super::permission`] (the inline real-time approval sheet),
//! this is a centered, dismissable overlay opened via the `/permissions` slash
//! command. It lists every cached "always allow" rule for the session, with
//! per-row revoke (`Space`) and a clear-all action (`c`).

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use super::common::{placeholder, selectable_row};
use crate::tui::render::Theme;
use crate::tui::render::primitives::{centered_rect, modal_frame, render_body, viewport_rect};

/// Draw the permissions manager modal: a centered, dismissable list of cached
/// "always allow" rules. Each row shows `<tool> <scope>`; `Space` revokes the
/// selected rule, `c` clears all. Data comes from the session-context snapshot
/// (the same one the `/session` modal used), refreshed after each mutation.
pub fn draw_permissions_manager(
    frame: &mut Frame,
    session_context: Option<&neenee_core::SessionContextSnapshot>,
    modal_index: usize,
    scroll: &mut usize,
    theme: &Theme,
) {
    let area = centered_rect(64, 60, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    // ── Header ──
    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "Permissions",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )])),
            h,
        );
    }

    // ── Body: the rule list ──
    let rules = session_context
        .map(|s| s.permissions.as_slice())
        .unwrap_or(&[]);
    let mut body: Vec<Line> = Vec::new();
    if rules.is_empty() {
        body.push(placeholder(
            "No always-allow rules cached this session.",
            session_context.is_some(),
            theme.muted(),
        ));
    } else {
        let body_w = f.body.width as usize;
        for (i, rule) in rules.iter().enumerate() {
            let summary = format!("{} {}", rule.tool, rule.scope);
            body.push(selectable_row(
                i,
                modal_index,
                &summary,
                "Space revokes",
                true,
                "allowed",
                "",
                body_w,
                theme,
            ));
        }
    }

    let follow = if rules.is_empty() {
        None
    } else {
        Some(modal_index)
    };
    render_body(frame, f.body, body, scroll, follow, false, theme);

    // ── Footer ──
    if let Some(fo) = f.footer {
        let hint = if rules.is_empty() {
            "Esc close".to_string()
        } else {
            "↑↓ select · Space revoke · c clear all · Esc close".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}
