//! Activity modal: section-specific overview of the current turn, pursuit, or todos.
//!
//! Each section is opened independently by clicking the corresponding segment
//! on the activity bar — there is no tab strip or Left/Right cycling.

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use super::common::todo_status_glyph_color;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{centered_rect, modal_frame, render_body, viewport_rect};

/// Inputs for [`draw_activity_modal`]. Carries everything the old always-pinned
/// pursuit bar, plan panel, and activity bar used to show, gathered into one
/// overlay so the footer is a single line. Fields are `Option`al so the modal
/// simply omits a section when there is nothing to report.
pub struct ActivityModalView<'a> {
    /// Which section to show (Activity or Todos). Each section is opened
    /// independently by clicking the corresponding segment on the activity bar.
    pub active_tab: crate::tui::ActivityTab,
    /// Active pursuit, if any. Shown as an objective line plus one row per
    /// checklist item.
    pub pursuit: Option<&'a neenee_core::Pursuit>,
    /// Live unified task list, if any. Shown as a header (done/total) plus
    /// one row per item with a status glyph.
    pub todos: Option<&'a neenee_core::TodoList>,
    /// The current turn's user prompt, if any. Shown in the Activity tab.
    pub user_prompt: Option<&'a str>,
    /// Harness turn counter (`turn N`).
    pub turn_count: u64,
    /// Current tool round within the turn (1-indexed; `0` before the first
    /// model request).
    pub current_round: u64,
    /// Session-review alert (ADR-0016), or empty when inactive.
    pub review_alert: &'a str,
    /// Display id of the currently active model.
    pub current_model: &'a str,
    /// Wall-clock instant the current turn started, or `None` between turns.
    pub turn_started_at: Option<std::time::Instant>,

    pub activity: &'a str,
}

/// The Activity modal: a scrollable overview of a single section (Activity or
/// Todos). The active section is determined by which activity-bar segment the
/// user clicked — there is no tab strip inside the modal.
pub fn draw_activity_modal(
    frame: &mut Frame,
    view: ActivityModalView<'_>,
    scroll: &mut usize,
    theme: &Theme,
) {
    let ActivityModalView {
        active_tab,
        pursuit,
        todos,
        user_prompt,
        turn_count,
        current_round,
        review_alert,
        current_model,
        turn_started_at,
        activity,
    } = view;

    let area = centered_rect(72, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    // ── Header: section title (no tab strip) ──
    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                active_tab.title(),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let muted = theme.muted();
    let mut lines: Vec<Line<'static>> = Vec::new();

    match active_tab {
        crate::tui::ActivityTab::Activity => {
            let mut have_section = false;

            // ── Pursuit ──
            if let Some(pursuit) = pursuit {
                have_section = true;
                lines.push(Line::from(vec![Span::styled(
                    "Pursuit",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                )]));
                let objective_label = if pursuit.is_complete {
                    "✓ complete · ".to_string()
                } else {
                    String::new()
                };
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("{}{}", objective_label, pursuit.objective),
                        Style::default().fg(theme.fg()),
                    ),
                ]));
            }

            // ── Prompt (current turn's user message) ──
            if let Some(prompt) = user_prompt.filter(|p| !p.is_empty()) {
                if have_section {
                    lines.push(Line::from(""));
                }
                have_section = true;
                lines.push(Line::from(vec![Span::styled(
                    "Prompt",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                )]));
                // The body wraps long lines (render_body wrap=true), so the
                // raw prompt is pushed as a single 2-indent line and neenee-tui
                // breaks it at the body width.
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(prompt.to_string(), Style::default().fg(theme.fg())),
                ]));
            }

            // ── Status (always shown) ──
            if have_section {
                lines.push(Line::from(""));
            }
            let idle = activity.is_empty() || activity == "idle";
            lines.push(Line::from(vec![Span::styled(
                "Status",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )]));

            if turn_count > 0 {
                let mut detail: Vec<Span<'static>> = vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(format!("turn {}", turn_count), Style::default().fg(muted)),
                ];
                if current_round >= 1 {
                    detail.push(Span::styled(" · ", Style::default().fg(muted)));
                    detail.push(Span::styled(
                        format!("round {}", current_round),
                        Style::default().fg(muted),
                    ));
                }
                if !current_model.is_empty() {
                    detail.push(Span::styled(" · ", Style::default().fg(muted)));
                    detail.push(Span::styled(
                        crate::tui::model_display_name(current_model),
                        Style::default().fg(muted),
                    ));
                }
                if let Some(started) = turn_started_at {
                    detail.push(Span::styled(" · ", Style::default().fg(muted)));
                    detail.push(Span::styled(
                        crate::tui::render::chrome::format_elapsed(started.elapsed()),
                        Style::default().fg(muted),
                    ));
                }
                lines.push(Line::from(detail));
            }

            let status_style = if idle {
                Style::default().fg(muted)
            } else {
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::ITALIC)
            };
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    if idle {
                        "idle".to_string()
                    } else {
                        activity.to_string()
                    },
                    status_style,
                ),
            ]));
            if !review_alert.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("⚠ {review_alert}"),
                        Style::default().fg(theme.warn()),
                    ),
                ]));
            }
        }

        crate::tui::ActivityTab::Todos => {
            if let Some(list) = todos.filter(|l| !l.items.is_empty()) {
                use neenee_core::TodoStatus;
                let done = list.count(TodoStatus::Completed);
                let total = list.items.len();
                lines.push(Line::from(vec![
                    Span::styled(
                        "Todos",
                        Style::default()
                            .fg(theme.brand())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {done}/{total}"), Style::default().fg(muted)),
                ]));
                for item in &list.items {
                    let glyph_color = todo_status_glyph_color(item.status, theme, muted);
                    // The todo content wraps at the body width (render_body
                    // wrap=true), so long task descriptions no longer spill
                    // past the right edge and vanish.
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(item.status.glyph(), Style::default().fg(glyph_color)),
                        Span::styled(" ", Style::default()),
                        Span::styled(item.content.clone(), Style::default().fg(theme.fg())),
                    ]));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "No todos.",
                    Style::default().fg(muted),
                )));
            }
        }
    }

    render_body(frame, f.body, lines, scroll, None, true, theme);

    if let Some(footer) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ scroll · Esc to close",
                Style::default().fg(theme.muted()),
            ))),
            footer,
        );
    }
}
