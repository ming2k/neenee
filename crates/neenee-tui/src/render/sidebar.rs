//! Right-side persistent sidebar. Rendered as a full-height column when the
//! terminal is wide enough (or when the user forces it on). Scrolls
//! independently from the main chat so persistent context — the active goal,
//! its checklist (plans/todos), the harness mode, the autonomous loop status,
//! and the token/time budget — stays visible while the conversation scrolls.
//!
//! The sidebar's content is intentionally narrow: it summarizes state that
//! otherwise lives in the header or in expanded cards. Rendering uses the same
//! `app_bg`/`panel_bg` palette as the rest of the TUI so it reads as a
//! first-class pane rather than an overlay.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Paragraph},
    Frame,
};

use neenee_core::{AgentMode, Goal, GoalChecklistStatus, GoalStatus};

use super::primitives::viewport_rect;
use super::theme::Theme;
#[cfg(test)]
use neenee_core::GoalChecklistItem;

/// Width of the sidebar pane (in terminal columns) when it is visible.
/// Picked to leave a comfortable main-chat width on a 132-column terminal
/// while still fitting wrapped objective / checklist text.
pub const SIDEBAR_WIDTH: u16 = 32;

/// Minimum terminal width at which the sidebar auto-shows. Below this the
/// sidebar is hidden unless the user explicitly toggles it on. The main chat
/// keeps at least `WIDTH - SIDEBAR_WIDTH` columns of usable width.
pub const SIDEBAR_AUTO_WIDTH: u16 = 132;

/// Vertical inset applied to the sidebar so its content does not touch the
/// top/bottom terminal frame (mirrors the chat viewport's 1-cell margin).
const SIDEBAR_V_INSET: u16 = 1;
/// Horizontal inset inside the sidebar pane.
const SIDEBAR_H_INSET: u16 = 1;

/// Input bundle for [`draw_sidebar`]. Everything here is already part of the
/// existing `ChatView`/`App` state; bundling it keeps the signature readable
/// and avoids plumbing a long parameter list through `draw_chat`.
pub struct SidebarView<'a> {
    pub current_provider: &'a str,
    pub current_model: &'a str,
    pub current_mode: AgentMode,
    pub current_goal: Option<&'a Goal>,
    /// Harness loop status string (e.g. `"idle"`, `"loop 3/8"`).
    pub loop_status: &'a str,
    /// Current scroll offset (in content lines) the caller is holding.
    pub scroll: usize,
    pub theme: &'a Theme,
}

/// Layout info returned by [`draw_sidebar`] so the app loop can clamp scroll
/// and route mouse events.
pub struct SidebarRender {
    /// Screen rect the sidebar occupied this frame. `None` when the sidebar
    /// was not rendered (terminal too narrow and not forced on).
    pub rect: Option<Rect>,
    /// Total content height (lines) independent of the viewport clip. Used by
    /// the app loop to clamp the scroll offset for the next frame.
    pub content_lines: usize,
    /// Visible height of the sidebar's inner viewport.
    pub view_height: u16,
}

impl SidebarRender {
    pub fn empty() -> Self {
        Self {
            rect: None,
            content_lines: 0,
            view_height: 0,
        }
    }
}

/// Draw the sidebar into `frame`. Returns `SidebarRender::empty()` when the
/// pane is hidden this frame.
pub fn draw_sidebar(frame: &mut Frame, view: SidebarView<'_>) -> SidebarRender {
    let SidebarView {
        current_provider,
        current_model,
        current_mode,
        current_goal,
        loop_status,
        scroll,
        theme,
    } = view;

    let outer = viewport_rect(frame);
    // Dock against the right edge of the viewport.
    let sidebar_outer = Rect {
        x: outer.x + outer.width.saturating_sub(SIDEBAR_WIDTH),
        y: outer.y,
        width: SIDEBAR_WIDTH,
        height: outer.height,
    };

    // Paint the pane background so the sidebar reads as a solid column. No
    // left separator rule is drawn: the `panel_bg` vs `app_bg` contrast marks
    // the boundary on its own.
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.panel_bg)),
        sidebar_outer,
    );

    // Inner content rect after insets.
    let inner = Rect {
        x: sidebar_outer.x + SIDEBAR_H_INSET,
        y: sidebar_outer.y + SIDEBAR_V_INSET,
        width: sidebar_outer
            .width
            .saturating_sub(2 * SIDEBAR_H_INSET)
            .max(1),
        height: sidebar_outer
            .height
            .saturating_sub(2 * SIDEBAR_V_INSET)
            .max(1),
    };

    // Build the full content as a list of lines, then clip to the viewport.
    // This keeps scrolling trivial (offset slicing) and avoids reflowing the
    // whole tree each frame.
    let content_width = inner.width as usize;
    let lines = build_sidebar_lines(
        content_width,
        current_provider,
        current_model,
        current_mode,
        current_goal,
        loop_status,
        theme,
    );

    let content_lines = lines.len();
    let view_height = inner.height as usize;
    let max_scroll = content_lines.saturating_sub(view_height);
    let scroll = scroll.min(max_scroll);
    let visible = lines.into_iter().skip(scroll).take(view_height);

    let paragraph = Paragraph::new(visible.collect::<Vec<Line>>());
    frame.render_widget(paragraph, inner);

    SidebarRender {
        rect: Some(sidebar_outer),
        content_lines,
        view_height: inner.height,
    }
}

/// Build the sidebar's full content as a flat list of styled lines. Wrapped
/// here (rather than ratatui's `wrap`) so the caller can count content lines
/// accurately for independent scrolling.
#[allow(clippy::too_many_arguments)]
fn build_sidebar_lines<'a>(
    content_width: usize,
    current_provider: &'a str,
    current_model: &'a str,
    current_mode: AgentMode,
    current_goal: Option<&'a Goal>,
    loop_status: &'a str,
    theme: &'a Theme,
) -> Vec<Line<'a>> {
    let width = content_width.max(1);
    let mut lines: Vec<Line<'a>> = Vec::new();

    let section = |title: &'a str| {
        Line::from(vec![Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.text_muted)
                .add_modifier(Modifier::BOLD),
        )])
    };
    let blank = || Line::from("");

    // Title row: brand + mode badge.
    let (mode_label, mode_color) = match current_mode {
        AgentMode::Plan => ("PLAN", theme.info),
        AgentMode::Build => ("BUILD", theme.primary),
    };
    lines.push(Line::from(vec![
        Span::styled(
            "neenee",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            mode_label,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(blank());

    // Model / provider.
    lines.push(section("MODEL"));
    push_wrapped(&mut lines, theme.text, current_model, width);
    push_wrapped(&mut lines, theme.text_muted, current_provider, width);
    lines.push(blank());

    // Loop status (only meaningful when not idle).
    if !loop_status.is_empty() && loop_status != "idle" {
        lines.push(section("LOOP"));
        let label = loop_status_to_display(loop_status);
        push_wrapped(&mut lines, theme.primary, &label, width);
        lines.push(blank());
    }

    // Goal + checklist.
    match current_goal {
        Some(goal) => {
            lines.push(section("GOAL"));
            let statuspill = goal_status_pill(goal.status, theme);
            let mut header = vec![Span::raw(" "), statuspill];
            header.push(Span::raw(" "));
            push_objective(&mut lines, theme, &goal.objective, width, Some(header));

            if !goal.checklist.is_empty() {
                lines.push(blank());
                let total = goal.checklist.len();
                let done = goal
                    .checklist
                    .iter()
                    .filter(|item| {
                        matches!(
                            item.status,
                            GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                        )
                    })
                    .count();
                lines.push(Line::from(vec![
                    Span::styled(
                        "Plans",
                        Style::default()
                            .fg(theme.text_muted)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {done}/{total}"),
                        Style::default().fg(theme.primary),
                    ),
                ]));
                for item in &goal.checklist {
                    push_checklist_item(&mut lines, theme, item.status, &item.content, width);
                }
            }

            // Budget progress (tokens and time), shown only when set.
            if let Some(budget) = goal.token_budget {
                if budget > 0 {
                    lines.push(blank());
                    lines.push(section("BUDGET"));
                    let used_pct = ((goal.tokens_used as f64) / (budget as f64) * 100.0) as u8;
                    let used_pct = used_pct.min(100);
                    push_budget_bar(
                        &mut lines,
                        theme,
                        "tok",
                        goal.tokens_used,
                        budget,
                        used_pct,
                        width,
                    );
                }
            }
            if goal.time_used_seconds > 0 {
                if goal.token_budget.is_none() {
                    lines.push(blank());
                    lines.push(section("BUDGET"));
                }
                push_budget_bar(
                    &mut lines,
                    theme,
                    "time",
                    goal.time_used_seconds,
                    0,
                    0,
                    width,
                );
            }
        }
        None => {
            lines.push(section("GOAL"));
            lines.push(Line::from(Span::styled(
                "no active goal",
                Style::default().fg(theme.text_muted),
            )));
            lines.push(blank());
            lines.push(section("PLANS"));
            lines.push(Line::from(Span::styled(
                "set a goal with /goal",
                Style::default().fg(theme.text_muted),
            )));
        }
    }

    lines
}

/// Append a soft-wrapped text block as one line per visual row. The text is
/// converted to owned spans (ratatui `Cow::Owned`) so callers may pass strings
/// shorter-lived than the resulting `Line<'a>` borrow.
fn push_wrapped<'a>(
    out: &mut Vec<Line<'a>>,
    color: ratatui::style::Color,
    text: &str,
    width: usize,
) {
    if width == 0 {
        return;
    }
    let mut current_len: usize = 0;
    let mut current: Vec<Span<'a>> = Vec::new();
    let flush = |out: &mut Vec<Line<'a>>, current: &mut Vec<Span<'a>>| {
        if !current.is_empty() {
            out.push(Line::from(std::mem::take(current)));
        }
    };

    // Greedy word wrap. Words longer than `width` are hard-split.
    for word in text.split_whitespace() {
        let mut remaining = word;
        while !remaining.is_empty() {
            let space = (current_len > 0) as usize;
            let room = width.saturating_sub(current_len + space);
            if room == 0 {
                flush(out, &mut current);
                current_len = 0;
                continue;
            }
            let take = remaining.chars().count().min(room);
            let chunk: String = remaining.chars().take(take).collect();
            remaining = &remaining[chunk.len()..];
            if space > 0 && current_len > 0 {
                current.push(Span::raw(" "));
                current_len += 1;
            }
            current.push(Span::styled(chunk, Style::default().fg(color)));
            current_len += take;
            if current_len >= width {
                flush(out, &mut current);
                current_len = 0;
            }
        }
    }
    flush(out, &mut current);
}

/// Push the objective text, optionally prefixed by a status pill row.
fn push_objective<'a>(
    out: &mut Vec<Line<'a>>,
    theme: &'a Theme,
    objective: &str,
    width: usize,
    prefix: Option<Vec<Span<'a>>>,
) {
    if width == 0 {
        return;
    }
    // First line carries the pill + as much objective text as fits.
    let prefix_spans = prefix.unwrap_or_default();
    let prefix_chars: usize = prefix_spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    let room = width.saturating_sub(prefix_chars).max(1);
    let mut chars = objective.chars().peekable();
    let mut first_line: Vec<Span<'a>> = prefix_spans;
    let mut taken = 0usize;
    while let Some(c) = chars.peek().copied() {
        if taken + 1 > room {
            break;
        }
        if c == '\n' {
            chars.next();
            break;
        }
        first_line.push(Span::styled(c.to_string(), Style::default().fg(theme.text)));
        taken += 1;
        chars.next();
    }
    out.push(Line::from(first_line));
    // Remaining lines wrap within the full width.
    let rest: String = chars.collect();
    if !rest.trim().is_empty() {
        push_wrapped(out, theme.text, &rest, width);
    }
}

/// Push a single checklist item with a status glyph.
fn push_checklist_item<'a>(
    out: &mut Vec<Line<'a>>,
    theme: &'a Theme,
    status: GoalChecklistStatus,
    content: &str,
    width: usize,
) {
    let (glyph, color) = match status {
        GoalChecklistStatus::Completed => ("✓", theme.success),
        GoalChecklistStatus::Cancelled => ("✗", theme.text_muted),
        GoalChecklistStatus::InProgress => ("◎", theme.primary),
        GoalChecklistStatus::Pending => ("○", theme.text_muted),
    };
    let prefix = format!("{glyph} ");
    let prefix_chars = prefix.chars().count();
    let room = width.saturating_sub(prefix_chars).max(1);
    let mut first_line: Vec<Span<'a>> = vec![Span::styled(prefix, Style::default().fg(color))];
    let mut chars = content.chars().peekable();
    let mut taken = 0usize;
    while let Some(c) = chars.peek().copied() {
        if taken + 1 > room {
            break;
        }
        if c == '\n' {
            chars.next();
            break;
        }
        let line_color = match status {
            GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled => theme.text_muted,
            _ => theme.text,
        };
        first_line.push(Span::styled(c.to_string(), Style::default().fg(line_color)));
        taken += 1;
        chars.next();
    }
    out.push(Line::from(first_line));
    let rest: String = chars.collect();
    if !rest.trim().is_empty() {
        let line_color = match status {
            GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled => theme.text_muted,
            _ => theme.text,
        };
        push_wrapped(out, line_color, &rest, width);
        // Compensate indentation on wrapped continuation rows by prepending a
        // whitespace prefix equal to the marker glyph width. We re-emit the
        // prefix as a separate leading span by post-processing the last line.
        if let Some(last) = out.last_mut() {
            let pad: String = " ".repeat(prefix_chars);
            let mut new_spans: Vec<Span<'a>> = Vec::with_capacity(last.spans.len() + 1);
            new_spans.push(Span::raw(pad));
            new_spans.append(&mut last.spans);
            last.spans = new_spans;
        }
    }
}

/// Render a thin ASCII budget bar. `current`/`total` are shown numerically;
/// `pct` drives the bar fill (0..=100). When `total == 0` only a numeric line
/// is drawn (used for time tracking without a hard cap).
fn push_budget_bar<'a>(
    out: &mut Vec<Line<'a>>,
    theme: &'a Theme,
    label: &'static str,
    current: i64,
    total: i64,
    pct: u8,
    width: usize,
) {
    let numeric = if total > 0 {
        format!("{label}  {current} / {total}")
    } else {
        // Time-only: render as seconds.
        let secs = current;
        let human = if secs >= 3600 {
            format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
        } else if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
        format!("{label}  {human}")
    };
    out.push(Line::from(Span::styled(
        numeric,
        Style::default().fg(theme.text),
    )));

    if total > 0 && width >= 4 {
        let bar_width = width.saturating_sub(2); // brackets
        let filled = ((pct as usize) * bar_width / 100).min(bar_width);
        let empty = bar_width - filled;
        let bar: String = format!("[{}{}]", "#".repeat(filled), "-".repeat(empty));
        let bar_color = if pct >= 90 {
            theme.error_fg
        } else if pct >= 75 {
            theme.warning
        } else {
            theme.success
        };
        out.push(Line::from(Span::styled(
            bar,
            Style::default().fg(bar_color),
        )));
    }
}

/// Coloured status pill span for a goal status.
fn goal_status_pill<'a>(status: GoalStatus, theme: &'a Theme) -> Span<'a> {
    let (text, color) = match status {
        GoalStatus::Active => ("ACTIVE", theme.success),
        GoalStatus::Paused => ("PAUSED", theme.warning),
        GoalStatus::Blocked => ("BLOCKED", theme.error_fg),
        GoalStatus::UsageLimited => ("USAGE", theme.warning),
        GoalStatus::BudgetLimited => ("BUDGET", theme.error_fg),
        GoalStatus::Complete => ("DONE", theme.primary),
    };
    Span::styled(
        format!("[{text}]"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// Map a raw harness loop status string to a sidebar-friendly label.
fn loop_status_to_display(loop_status: &str) -> String {
    if loop_status.is_empty() || loop_status == "idle" {
        return String::new();
    }
    loop_status.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    #[test]
    fn sidebar_renders_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = theme();
        let backend = TestBackend::new(SIDEBAR_AUTO_WIDTH, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let goal = Goal {
            objective: "Ship the sidebar feature end to end".to_string(),
            status: GoalStatus::Active,
            checklist: vec![
                GoalChecklistItem {
                    content: "Design layout".to_string(),
                    status: GoalChecklistStatus::Completed,
                },
                GoalChecklistItem {
                    content: "Implement renderer".to_string(),
                    status: GoalChecklistStatus::InProgress,
                },
                GoalChecklistItem {
                    content: "Wire up scroll routing".to_string(),
                    status: GoalChecklistStatus::Pending,
                },
            ],
            tokens_used: 12_000,
            token_budget: Some(50_000),
            time_used_seconds: 240,
        };
        terminal
            .draw(|f| {
                let _ = draw_sidebar(
                    f,
                    SidebarView {
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: Some(&goal),
                        loop_status: "loop 3/8",
                        scroll: 0,
                        theme: &theme,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn sidebar_renders_without_goal() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = theme();
        let backend = TestBackend::new(SIDEBAR_AUTO_WIDTH, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_sidebar(
                    f,
                    SidebarView {
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Plan,
                        current_goal: None,
                        loop_status: "idle",
                        scroll: 0,
                        theme: &theme,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn sidebar_handles_long_objective_and_checklist() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = theme();
        let backend = TestBackend::new(SIDEBAR_AUTO_WIDTH, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let long = "X".repeat(200);
        let goal = Goal {
            objective: long.clone(),
            status: GoalStatus::Active,
            checklist: vec![
                GoalChecklistItem {
                    content: long.clone(),
                    status: GoalChecklistStatus::Pending,
                },
                GoalChecklistItem {
                    content: long,
                    status: GoalChecklistStatus::InProgress,
                },
            ],
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
        };
        // Scroll past the start to exercise the skip/take path.
        terminal
            .draw(|f| {
                let _ = draw_sidebar(
                    f,
                    SidebarView {
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: Some(&goal),
                        loop_status: "",
                        scroll: 5,
                        theme: &theme,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn budget_bar_scales_within_width() {
        let mut lines = Vec::new();
        let theme = theme();
        // 80%: on a 20-wide pane (bar_width=18) → filled = 14, empty = 4.
        push_budget_bar(&mut lines, &theme, "tok", 40_000, 50_000, 80, 20);
        assert_eq!(lines.len(), 2);
        let content = &lines[1].spans[0].content;
        let s: &str = content.as_ref();
        assert!(s.starts_with('['));
        assert!(s.ends_with(']'));
        assert_eq!(s.len(), 20);
        let filled = s.matches('#').count();
        assert_eq!(filled, 14);
    }

    #[test]
    fn time_only_budget_skips_bar() {
        let mut lines = Vec::new();
        let theme = theme();
        push_budget_bar(&mut lines, &theme, "time", 125, 0, 0, 20);
        assert_eq!(lines.len(), 1);
    }
}
