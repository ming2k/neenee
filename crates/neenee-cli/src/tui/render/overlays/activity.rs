//! Activity modal: tabbed overview of the current turn, pursuit, and tasks.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::common::todo_status_glyph_color;
use crate::tui::render::primitives::{centered_rect, modal_frame, render_body, viewport_rect};
use crate::tui::render::Theme;

/// Inputs for [`draw_activity_modal`]. Carries everything the old always-pinned
/// pursuit bar, plan panel, and activity bar used to show, gathered into one
/// overlay so the footer is a single line. Fields are `Option`al so the modal
/// simply omits a section when there is nothing to report.
pub struct ActivityModalView<'a> {
    /// Active tab (Activity / Tasks).
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

/// The Activity modal: a scrollable overview of the current pursuit, the live
/// plan-progress breakdown, and the running turn/round/model/elapsed/status.
/// Replaces the always-pinned pursuit bar + plan panel (which have moved into the
/// scrolling transcript as inline notices) so the footer stays a single line;
/// the activity bar remains pinned as the click target that opens this modal.
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

    // ── Header: title + tab strip ──
    if let Some(h) = f.header {
        let mut header_spans: Vec<Span> = vec![
            Span::styled(
                "Activity",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
        ];
        for (i, variant) in crate::tui::ActivityTab::ALL.iter().enumerate() {
            if i > 0 {
                header_spans.push(Span::raw("  "));
            }
            let active = *variant == active_tab;
            let style = if active {
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(theme.muted())
            };
            header_spans.push(Span::styled(variant.label(), style));
        }
        frame.render_widget(Paragraph::new(Line::from(header_spans)), h);
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
                for wrap_line in crate::tui::render::text_layout::wrap_text(prompt, 68) {
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(wrap_line.text.clone(), Style::default().fg(theme.fg())),
                    ]));
                }
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

        crate::tui::ActivityTab::Tasks => {
            if let Some(list) = todos.filter(|l| !l.items.is_empty()) {
                use neenee_core::TodoStatus;
                let done = list.count(TodoStatus::Completed);
                let total = list.items.len();
                lines.push(Line::from(vec![
                    Span::styled(
                        "Tasks",
                        Style::default()
                            .fg(theme.brand())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {done}/{total}"), Style::default().fg(muted)),
                ]));
                for item in &list.items {
                    let glyph_color = todo_status_glyph_color(item.status, theme, muted);
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(item.status.glyph(), Style::default().fg(glyph_color)),
                        Span::styled(" ", Style::default()),
                        Span::styled(item.content.clone(), Style::default().fg(theme.fg())),
                    ]));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "No tasks.",
                    Style::default().fg(muted),
                )));
            }
        }
    }

    render_body(frame, f.body, lines, scroll, None);

    if let Some(footer) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "←/→ tabs · ↑/↓ scroll · Esc to close",
                Style::default().fg(theme.muted()),
            ))),
            footer,
        );
    }
}
