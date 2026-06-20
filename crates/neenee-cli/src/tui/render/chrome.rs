//! Transient chrome around the input box: the activity status bar with an
//! animated breathing-dot indicator shown above the input, the completion menu
//! anchored above the input, and the persistent hint bar pinned below the
//! input.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::document::{estimate_context_tokens, TranscriptMessage};
use crate::tui::input::FocusZone;
use crate::tui::layout::LayoutMap;
use neenee_core::{Goal, GoalStatus};

use super::design::{
    HINT_BAR_GAP_MIN, HINT_BAR_GOAL_MAX_CHARS, HINT_BAR_INNER_PADDING, HINT_BAR_SEGMENT_GAP,
};
use super::primitives::{contrast_fg, viewport_rect};
use super::Theme;

/// Number of distinct luminance steps in one breathing cycle. At the 100ms
/// spinner tick this is ~1.2s per cycle — calm, not frantic.
pub const SPINNER_PHASES: usize = 12;

/// The activity indicator glyph: a single dot whose luminance breathes (see
/// [`breathing_color`]) rather than a cycling braille frame. Replaces the old
/// braille spinner for a quieter, less busy feel.
pub fn spinner_glyph() -> &'static str {
    "●"
}

/// Cosine luminance sweep between `bg` (dim, at phase 0) and `base` (bright,
/// at mid-cycle). Used with [`spinner_glyph`] so the running indicator is a
/// dot that gently breathes instead of a spinning braille glyph.
pub fn breathing_color(phase: usize, base: Color, bg: Color) -> Color {
    let (br, bgc, bb) = rgb_of(bg);
    let (fr, fgc, fb) = rgb_of(base);
    let n = SPINNER_PHASES as f32;
    let t = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * (phase % SPINNER_PHASES) as f32 / n).cos();
    Color::Rgb(lerp_u8(br, fr, t), lerp_u8(bgc, fgc, t), lerp_u8(bb, fb, t))
}

fn rgb_of(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (119, 125, 117), // text_muted fallback for non-truecolor
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Draw the transient activity bar that sits directly above the input box.
/// Replaces the old inline `┃ neenee ⟳ <status>` indicator: the brand prefix
/// is dropped (the header already shows it) and the static `⟳` glyph is
/// replaced by a breathing-dot indicator so the harness never looks frozen.
pub fn draw_status_bar(
    frame: &mut Frame,
    rect: Rect,
    status: &str,
    spinner_phase: usize,
    theme: &Theme,
) {
    if status.is_empty() || status == "idle" || status == "responding" {
        return;
    }
    let spinner = spinner_glyph();
    let spinner_color = breathing_color(spinner_phase, theme.brand(), theme.surface());
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            spinner,
            Style::default()
                .fg(spinner_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            status,
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::ITALIC),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

/// Draw a completion menu anchored above the input box. Renders each
/// candidate as `label · description` with the selected row highlighted; no
/// title or operating instructions are shown so the menu reads as a plain
/// list of candidates. Works for both slash-command and `@path` mention
/// completions since the rendering only depends on the label/description
/// pair, not the replacement range.
pub fn draw_completion_menu(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    completions: &[crate::tui::Completion],
    selected_idx: Option<usize>,
    anchor: Rect,
    theme: &Theme,
) {
    if completions.is_empty() {
        return;
    }

    const MAX_VISIBLE: usize = 6;

    let visible_count = completions.len().min(MAX_VISIBLE);
    let popup_height = visible_count as u16;

    // Compute width from content. The description column is dropped entirely
    // (separator + padding) when no candidate carries a description — the
    // `@path` menu uses empty descriptions for a plain list of paths,
    // matching opencode's minimal aesthetic.
    let any_desc = completions.iter().any(|c| !c.description.is_empty());
    let max_cmd = completions
        .iter()
        .map(|c| c.label.width())
        .max()
        .unwrap_or(0);
    let max_desc = if any_desc {
        completions
            .iter()
            .map(|c| c.description.width())
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let inner_width = if any_desc {
        (max_cmd + 3 + max_desc).max(30) as u16
    } else {
        (max_cmd + 2).max(20) as u16
    };
    let popup_width = inner_width + 4; // left bar + right padding

    // Position: try above the input box; if not enough room, clamp to top.
    let mut y = anchor.y.saturating_sub(popup_height);
    if y == 0 && anchor.y < popup_height {
        y = 0;
    }
    let viewport = viewport_rect(frame);
    let x = anchor
        .x
        .saturating_add(2)
        .min(viewport.right().saturating_sub(popup_width));

    let area = Rect::new(x, y, popup_width.min(viewport.right() - x), popup_height);
    frame.render_widget(Clear, area);

    let block = RtBlock::default()
        .borders(Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(theme.brand()))
        .style(Style::default().bg(theme.body()));

    let lines: Vec<Line> = completions
        .iter()
        .take(MAX_VISIBLE)
        .enumerate()
        .map(|(i, c)| {
            let is_selected = Some(i) == selected_idx;
            let style = if is_selected {
                Style::default()
                    .bg(theme.brand())
                    .fg(contrast_fg(theme.brand()))
            } else {
                Style::default().fg(theme.fg())
            };
            let cmd_style = if is_selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
            };
            // Build the row spans. When the description is empty (e.g. the
            // `@path` menu), drop the `· desc` suffix entirely so the row is just
            // the candidate — matches the "candidate + description" plain style
            // without forcing every row to carry a `·` ornament.
            let mut spans = vec![
                Span::styled(" ", Style::default()),
                Span::styled(format!("{:<width$}", c.label, width = max_cmd), cmd_style),
            ];
            if any_desc {
                spans.push(Span::styled("· ", Style::default().fg(theme.muted())));
                spans.push(Span::styled(
                    format!("{:<width$}", c.description, width = max_desc),
                    if is_selected {
                        style
                    } else {
                        Style::default().fg(theme.muted())
                    },
                ));
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Layout info returned by [`draw_hint_bar`] for hit-testing. The goal segment
/// is the only interactive piece (clicking it surfaces the full checklist via
/// `/goal status`), so its screen rect is reported back to the app.
#[derive(Default, Clone, Copy)]
pub struct HintBarLayout {
    /// Screen rect of the goal/checklist segment, when one is rendered.
    pub goal_rect: Option<Rect>,
}

/// Inputs for [`draw_hint_bar`]. Mirrors the data the old top header carried so
/// the same model / goal / MCP / context-usage info survives the move to the
/// bottom strip.
pub struct HintBarView<'a> {
    pub current_provider: &'a str,
    pub current_model: &'a str,
    pub current_goal: Option<&'a Goal>,
    pub messages: &'a [TranscriptMessage],
    pub mcp_statuses: &'a [(String, neenee_core::mcp::McpConnectionStatus)],
    /// Which surface owns keyboard focus this frame. Rendered as a colored
    /// pill at the start of the bar so the user can always tell whether the
    /// next keypress lands in the input box (Compose) or the conversation
    /// stream (Browse).
    pub focus_zone: FocusZone,
    /// True while the prompt is a `!`-prefixed shell command (Compose zone
    /// only). Promotes the pill to `[ SHELL ]` in the warning tone so the
    /// user can tell at a glance the next Enter runs the rest of the line
    /// directly through the bash tool — no LLM roundtrip. Orthogonal to
    /// `focus_zone`: Browse always wins, so a stale `!` left in the box
    /// while navigating the transcript does not flash a misleading SHELL.
    pub shell_active: bool,
    /// Whether auto-approve mode is active. Renders an extra warning-toned
    /// pill so the elevated, no-prompt state is unmissable.
    pub auto_approve: bool,
}

/// Draw the single-line hint bar pinned below the input box. Carries the same
/// model / goal / MCP / context-usage info that the old top header showed, now
/// collapsed onto one row so the transcript reclaims the vertical space.
///
/// Layout: focus-zone pill (and optional auto-approve pill) on the left,
/// right-aligned cluster of `model · goal · MCP · context-usage` on the right.
/// The goal segment is rendered against a subtly raised background so the user
/// can tell it is clickable; clicking anywhere inside that rect surfaces the
/// full goal via `/goal status`.
pub fn draw_hint_bar(
    frame: &mut Frame,
    rect: Rect,
    view: HintBarView<'_>,
    theme: &Theme,
) -> HintBarLayout {
    let HintBarView {
        current_provider,
        current_model,
        current_goal,
        messages,
        mcp_statuses,
        focus_zone,
        shell_active,
        auto_approve,
    } = view;

    let bg = theme.surface();
    let accent_bg = theme.raised();
    let full_w = rect.width as usize;

    // --- Focus-zone pill (leftmost). Renders as `[ COMPOSE ]` / `[ BROWSE ]`
    // / `[ SHELL ]` against the raised element background so it reads as a
    // surface badge even at a glance. The active state takes a distinct
    // color so the user can tell which surface the next keypress will land
    // on without reading the label:
    //   - BROWSE  → warn tone (keyboard focus is on the transcript stream)
    //   - SHELL   → warn tone (Compose zone, but the next Enter bypasses
    //                          the LLM and runs `!…` via the bash tool)
    //   - COMPOSE → brand tone (default chat-to-LLM prompt)
    // SHELL is suppressed outside the Compose zone so a stale `!` left in
    // the box while navigating the transcript does not flash a misleading
    // badge — Browse always wins.
    let (zone_label, zone_fg) = if focus_zone.is_browse() {
        ("BROWSE", theme.warn())
    } else if shell_active {
        ("SHELL", theme.warn())
    } else {
        ("COMPOSE", theme.brand())
    };
    let zone_text = format!(" {} ", zone_label);
    let zone_pill_width = zone_text.width() + 2; // +2 for the surrounding brackets
    let zone_spans = vec![
        Span::styled("[", Style::default().fg(zone_fg).bg(accent_bg)),
        Span::styled(
            zone_text,
            Style::default()
                .fg(zone_fg)
                .bg(accent_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("]", Style::default().fg(zone_fg).bg(accent_bg)),
    ];

    // --- Auto-approve pill (only when active). Mirrors the focus-zone pill
    // styling but in the warning tone so the elevated, no-prompt state is
    // unmissable. Rendered against the same raised background so it reads as
    // a sibling badge rather than body text.
    let warn_fg = theme.warn();
    let auto_pill_text = " AUTO-APPROVE ";
    let auto_pill_width = auto_pill_text.chars().count() + 2; // +2 for brackets
    let auto_spans: Vec<Span<'static>> = if auto_approve {
        vec![
            Span::styled("[", Style::default().fg(warn_fg).bg(accent_bg)),
            Span::styled(
                auto_pill_text,
                Style::default()
                    .fg(warn_fg)
                    .bg(accent_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("]", Style::default().fg(warn_fg).bg(accent_bg)),
        ]
    } else {
        Vec::new()
    };
    let auto_segment_width = if auto_approve {
        HINT_BAR_SEGMENT_GAP + auto_pill_width
    } else {
        0
    };

    // --- Right cluster: model name, goal (clickable), MCP, context bar.
    // Build each segment separately so we can drop optional ones (MCP, goal)
    // when the terminal is too narrow, and so we can record the goal rect for
    // hit-testing.
    let checklist = current_goal.and_then(goal_checklist_summary);
    let goal_segment: Option<(String, usize)> = current_goal.map(|goal| {
        let objective: String = goal
            .objective
            .chars()
            .take(HINT_BAR_GOAL_MAX_CHARS)
            .collect();
        let suffix = if goal.objective.chars().count() > HINT_BAR_GOAL_MAX_CHARS {
            "..."
        } else {
            ""
        };
        let mark = if goal.status == GoalStatus::Complete {
            "✓"
        } else {
            "◎"
        };
        let progress = checklist
            .as_ref()
            .map(|(done, total, _)| format!(" [{}/{}]", done, total))
            .unwrap_or_default();
        let label = format!("{} {}{}{}", mark, objective, suffix, progress);
        let width = label.width();
        (label, width)
    });

    let mcp_summary = format_mcp_summary(mcp_statuses);
    let mcp_width = mcp_summary.width();

    let context_max = crate::tui::model_context_window(current_provider);

    // Left side: focus-zone pill and optional auto-approve pill. Computed now
    // so the gap to the right cluster can hug the right edge.
    let inner = HINT_BAR_INNER_PADDING;
    let left_used = inner + zone_pill_width + auto_segment_width;

    // Reserve the right-side segments from the inside out: model is always
    // shown; then goal; then MCP; then context bar. Each preceding segment is
    // separated by `HINT_BAR_SEGMENT_GAP`. The context bar is appended last so
    // we can shrink it to fit the row (see below). We then compute the gap
    // between cwd and the right cluster so the cluster hugs the right edge.
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    let mut right_width = 0usize;

    let mut goal_start_x: Option<usize> = None;
    let mut goal_end_x: Option<usize> = None;

    // Model name (always present). Resolve the friendly preset name (e.g.
    // `DeepSeek Pro (R1)`) from the provider id so the always-visible indicator
    // matches the `/models` picker instead of leaking the raw model id
    // (`deepseek-reasoner`); fall back to the model id for non-preset providers.
    let model_label = crate::tui::model_display_name(current_provider, current_model);
    right_width += model_label.width();
    right_spans.push(Span::styled(
        model_label,
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD)
            .bg(bg),
    ));

    // Goal (clickable). Rendered against `accent_bg` so the user can tell it
    // is interactive; we track its absolute x-range for hit-testing.
    if let Some((label, w)) = goal_segment.as_ref() {
        right_spans.push(Span::styled(
            " ".repeat(HINT_BAR_SEGMENT_GAP),
            Style::default().bg(bg),
        ));
        right_width += HINT_BAR_SEGMENT_GAP;
        goal_start_x = Some(right_width);
        right_spans.push(Span::styled(
            label.clone(),
            Style::default().fg(theme.muted()).bg(accent_bg),
        ));
        right_width += *w;
        goal_end_x = Some(right_width);
        right_spans.push(Span::styled(
            " ".repeat(HINT_BAR_SEGMENT_GAP),
            Style::default().bg(bg),
        ));
        right_width += HINT_BAR_SEGMENT_GAP;
    }

    if !mcp_summary.is_empty() {
        right_spans.push(Span::styled(
            mcp_summary,
            Style::default().fg(theme.muted()).bg(bg),
        ));
        right_width += mcp_width;
    }

    // Context-usage segment: `89.2k (8%)`. Always shown when the model
    // reports a context window; the percentage takes the threshold color so
    // a nearly full window is unmissable.
    if context_max > 0 {
        let used = estimate_context_tokens(messages);
        let ctx_spans = context_usage_spans(used, context_max, theme, bg);
        let ctx_width: usize = ctx_spans.iter().map(|s| s.content.width()).sum();
        right_spans.push(Span::styled(
            " ".repeat(HINT_BAR_SEGMENT_GAP),
            Style::default().bg(bg),
        ));
        right_width += HINT_BAR_SEGMENT_GAP;
        right_spans.extend(ctx_spans);
        right_width += ctx_width;
    }

    let gap = full_w
        .saturating_sub(left_used + right_width)
        .max(HINT_BAR_GAP_MIN);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(8 + right_spans.len());
    spans.push(Span::styled(" ".repeat(inner), Style::default().bg(bg)));
    spans.extend(zone_spans);
    if auto_approve {
        spans.push(Span::styled(
            " ".repeat(HINT_BAR_SEGMENT_GAP),
            Style::default().bg(bg),
        ));
        spans.extend(auto_spans);
    }
    spans.push(Span::styled(" ".repeat(gap), Style::default().bg(bg)));
    spans.extend(right_spans);
    // Trailing fill so the row owns every cell on this line.
    let used = left_used + gap + right_width;
    spans.push(Span::styled(
        " ".repeat(full_w.saturating_sub(used)),
        Style::default().bg(bg),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), rect);

    // Map the goal segment's in-cluster offset to absolute screen coordinates
    // for hit-testing. `goal_start_x` is measured from the start of the right
    // cluster; the cluster begins at `inner + zone_pill_width +
    // auto_segment_width + gap`.
    let goal_rect = if let (Some(start), Some(end)) = (goal_start_x, goal_end_x) {
        let cluster_x = rect.x as usize + left_used + gap;
        let x0 = (cluster_x + start).min(rect.x as usize + full_w);
        let x1 = (cluster_x + end).min(rect.x as usize + full_w);
        if x1 > x0 {
            Some(Rect::new(x0 as u16, rect.y, (x1 - x0) as u16, 1))
        } else {
            None
        }
    } else {
        None
    };

    HintBarLayout { goal_rect }
}

fn goal_checklist_summary(goal: &Goal) -> Option<(usize, usize, String)> {
    if goal.checklist.is_empty() {
        return None;
    }
    let done = goal
        .checklist
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                neenee_core::GoalChecklistStatus::Completed
                    | neenee_core::GoalChecklistStatus::Cancelled
            )
        })
        .count();
    let current = goal
        .checklist
        .iter()
        .find(|item| item.status == neenee_core::GoalChecklistStatus::InProgress)
        .or_else(|| {
            goal.checklist
                .iter()
                .find(|item| item.status == neenee_core::GoalChecklistStatus::Pending)
        })
        .or_else(|| goal.checklist.last())
        .map(|item| item.content.clone())
        .unwrap_or_default();
    Some((done, goal.checklist.len(), current))
}

fn format_mcp_summary(statuses: &[(String, neenee_core::mcp::McpConnectionStatus)]) -> String {
    if statuses.is_empty() {
        return String::new();
    }
    let total = statuses.len();
    let connected = statuses
        .iter()
        .filter(|(_, status)| {
            matches!(
                status,
                neenee_core::mcp::McpConnectionStatus::Connected { .. }
            )
        })
        .count();
    let tools: usize = statuses
        .iter()
        .filter_map(|(_, status)| match status {
            neenee_core::mcp::McpConnectionStatus::Connected { tools } => Some(*tools),
            _ => None,
        })
        .sum();
    format!("MCP {}/{} · {} tools", connected, total, tools)
}

/// Context-usage ratio at which the usage bar turns from green to yellow.
const CONTEXT_USAGE_WARN_THRESHOLD: f64 = 0.7;
/// Context-usage ratio at which the usage bar turns from yellow to red.
const CONTEXT_USAGE_CRIT_THRESHOLD: f64 = 0.9;

/// Format a token count with a single-letter SI suffix: `999`, `1.0k`, `20.2k`,
/// `1.5M`, `3.2B`.
fn format_token_count(n: usize) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

/// Context-window usage indicator: `89.2k (8%)`. The percentage takes the
/// green → yellow → red threshold color so a nearly full window is
/// unmissable; the token count stays muted. `bg` is applied to every span so
/// the indicator reads on a solid background.
fn context_usage_spans(
    used: usize,
    max: usize,
    theme: &Theme,
    bg: Color,
) -> Vec<Span<'static>> {
    let ratio = if max == 0 {
        0.0
    } else {
        ((used as f64) / (max as f64)).clamp(0.0, 1.0)
    };
    let color = if ratio < CONTEXT_USAGE_WARN_THRESHOLD {
        theme.ok()
    } else if ratio < CONTEXT_USAGE_CRIT_THRESHOLD {
        theme.warn()
    } else {
        theme.err()
    };
    let pct = (ratio * 100.0).round() as u32;

    vec![
        Span::styled(
            format_token_count(used),
            Style::default().fg(theme.muted()).bg(bg),
        ),
        Span::styled(
            format!(" ({}%)", pct),
            Style::default().fg(color).bg(bg),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_summary_counts_connected_servers_and_tools() {
        use neenee_core::mcp::McpConnectionStatus;
        let statuses = vec![
            (
                "fs".to_string(),
                McpConnectionStatus::Connected { tools: 3 },
            ),
            (
                "git".to_string(),
                McpConnectionStatus::Failed("not found".to_string()),
            ),
        ];
        assert_eq!(
            format_mcp_summary(&statuses),
            "MCP 1/2 · 3 tools".to_string()
        );
    }

    #[test]
    fn mcp_summary_is_empty_when_no_servers() {
        assert!(format_mcp_summary(&[]).is_empty());
    }

    #[test]
    fn format_token_count_uses_si_suffixes() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1.0k");
        assert_eq!(format_token_count(20_200), "20.2k");
        assert_eq!(format_token_count(1_000_000), "1.0M");
        assert_eq!(format_token_count(3_200_000_000), "3.2B");
    }

    #[test]
    fn context_usage_spans_render_used_and_percentage() {
        let theme = Theme::default();
        let spans = context_usage_spans(20_200, 256_000, &theme, theme.panel());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "20.2k (8%)");
    }

    #[test]
    fn checklist_summary_prefers_current_work() {
        let goal = Goal {
            objective: "ship".to_string(),
            status: GoalStatus::Active,
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
            checklist: vec![
                neenee_core::GoalChecklistItem {
                    content: "implemented".to_string(),
                    status: neenee_core::GoalChecklistStatus::Completed,
                },
                neenee_core::GoalChecklistItem {
                    content: "run tests".to_string(),
                    status: neenee_core::GoalChecklistStatus::InProgress,
                },
            ],
        };

        assert_eq!(
            goal_checklist_summary(&goal),
            Some((1, 2, "run tests".to_string()))
        );
    }

    /// The hint bar must render the model, goal, MCP, and context bar on
    /// a single line below the input without panicking, and must report a
    /// non-empty goal rect when a goal is present so click handling can route
    /// to `/goal status`.
    #[test]
    fn hint_bar_renders_all_segments_and_reports_goal_rect() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let messages = vec![TranscriptMessage::new(neenee_core::Role::User, "hi")];
        let goal = Goal {
            objective: "ship the hint bar".to_string(),
            status: GoalStatus::Active,
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
            checklist: vec![],
        };
        let mut goal_rect = None;
        terminal
            .draw(|f| {
                goal_rect = draw_hint_bar(
                    f,
                    Rect::new(0, 2, 80, 1),
                    HintBarView {
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_goal: Some(&goal),
                        messages: &messages,
                        mcp_statuses: &[],
                        focus_zone: crate::tui::input::FocusZone::Compose,
                        shell_active: false,
                        auto_approve: false,
                    },
                    &theme,
                )
                .goal_rect;
            })
            .unwrap();
        assert!(goal_rect.is_some(), "goal rect should be reported");
    }

    #[test]
    fn hint_bar_pill_reflects_focus_zone_and_shell_active() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let messages: Vec<TranscriptMessage> = vec![];
        // Helper that draws the bar with a given (focus_zone, shell_active)
        // pair and returns the pill text — i.e. the bracketed label in the
        // top-left of the rendered buffer.
        fn pill_text(
            terminal: &mut Terminal<ratatui::backend::TestBackend>,
            focus_zone: crate::tui::input::FocusZone,
            shell_active: bool,
        ) -> String {
            let mut captured = String::new();
            terminal
                .draw(|f| {
                    let view = HintBarView {
                        current_provider: "",
                        current_model: "",
                        current_goal: None,
                        messages: &Vec::<TranscriptMessage>::new(),
                        mcp_statuses: &[],
                        focus_zone,
                        shell_active,
                        auto_approve: false,
                    };
                    draw_hint_bar(f, Rect::new(0, 0, 80, 1), view, &Theme::default());
                })
                .unwrap();
            // The pill starts at column 0: `[ LABEL ]`. Walk the row and
            // collect the bracketed region verbatim.
            let buf = terminal.backend().buffer();
            let bw = buf.area.width as usize;
            for x in 0..bw {
                let cell = &buf.content[x];
                captured.push_str(cell.symbol());
            }
            let trimmed = captured.trim_start();
            let end = trimmed
                .find(']')
                .map(|i| i + 1)
                .unwrap_or_else(|| trimmed.len().min(12));
            trimmed[..end].trim().to_string()
        }

        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let _ = &messages;

        // Compose zone, no `!` typed → COMPOSE.
        assert_eq!(
            pill_text(&mut terminal, crate::tui::input::FocusZone::Compose, false),
            "[ COMPOSE ]"
        );
        // Compose zone, `!`-prefixed input → SHELL promotion.
        assert_eq!(
            pill_text(&mut terminal, crate::tui::input::FocusZone::Compose, true),
            "[ SHELL ]"
        );
        // Browse zone always wins even if a stale `!` is in the box, so the
        // pill does not flash a misleading SHELL while navigating.
        assert_eq!(
            pill_text(&mut terminal, crate::tui::input::FocusZone::Browse, true),
            "[ BROWSE ]"
        );
        assert_eq!(
            pill_text(&mut terminal, crate::tui::input::FocusZone::Browse, false),
            "[ BROWSE ]"
        );
        let _ = theme;
    }
}
