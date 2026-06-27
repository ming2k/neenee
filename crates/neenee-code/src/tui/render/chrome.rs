//! Transient chrome around the input box: the activity bar with an
//! animated breathing-dot indicator shown above the input, the completion menu
//! anchored above the input, and the persistent hint bar pinned below the
//! input. The activity bar is also the click target that opens the Activity
//! modal (pursuit + plan + live activity), replacing the old always-pinned pursuit
//! bar and task panel.

use neenee_tui::{
    Block as RtBlock, Clear, Color, Frame, Line, Modifier, Paragraph, Rect, Span, Style,
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use crate::tui::document::{TranscriptMessage, estimate_context_tokens};
use crate::tui::layout::LayoutMap;

use super::Theme;
use super::design::{HINT_BAR_GAP_MIN, HINT_BAR_INNER_PADDING, HINT_BAR_SEGMENT_GAP};
use super::primitives::{contrast_fg, viewport_rect};

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

/// Hit-test information returned by [`draw_activity_bar`]. Carries the
/// screen rect of the full bar plus an optional sub-rect covering the
/// `todos d/t` segment so the event loop can route a click on the todos
/// badge directly to the Todos section of the Activity modal.
pub struct ActivityBarHit {
    /// The full bar rect (click → Activity modal, Activity section).
    pub bar_rect: Rect,
    /// The `todos d/t` badge sub-rect (click → Activity modal, Todos section).
    /// `None` when no todos are shown (empty task list).
    pub todos_rect: Option<Rect>,
}

/// Draw the transient activity bar that sits directly above the input box.
/// Replaces the old inline `┃ neenee ⟳ <status>` indicator: the brand prefix
/// is dropped (the header already shows it) and the static `⟳` glyph is
/// replaced by a breathing-dot indicator so the harness never looks frozen.
///
/// Layout:
/// ```text
/// active:  <spinner> <status> [· ⟴ <pursuit>] [· <elapsed>] [⚠ <alert>]      todos d/t
/// idle:                                                                   todos d/t
/// ```
/// The left half is transient (turn-scoped); the right-pinned `todos d/t`
/// badge is persistent and shows whenever a non-empty task list exists — even
/// when the harness is idle.
///
/// The bar surfaces what the user most wants to know mid-turn — the live
/// status, whether a pursuit/plan is in flight, and how long the turn has
/// run — and is the click target that opens the Activity modal for the full
/// detail. Each segment is independently clickable: a click on the `todos`
/// badge opens the Todos section directly, while a click anywhere else opens
/// the Activity section. The structural counters (`turn N · round M ·
/// <model>`) live in the modal: they change rarely and take space, while the
/// bar is a glance surface. Segments are omitted when there is nothing to
/// report:
/// - pursuit badge only when a pursuit is armed (`⟴ <truncated objective>`);
/// - elapsed only while the turn timer is running;
/// - the whole left half only while a turn is active.
///
/// When the status string already carries a reason (e.g.
/// `retry 1/4 in 3s · <message>`), it flows through unchanged as the lead.
///
/// Returns `Some(ActivityBarHit)` when the bar is drawn so the event loop
/// can hit-test clicks and open the Activity modal; `None` when the bar is
/// hidden (no transient activity AND no todos).
#[allow(clippy::too_many_arguments)]
pub fn draw_activity_bar(
    frame: &mut Frame,
    rect: Rect,
    pursuit: Option<&neenee_core::Pursuit>,
    todos: Option<&neenee_core::TodoList>,
    review_alert: &str,
    turn_started_at: Option<Instant>,
    status: &str,
    spinner_phase: usize,
    theme: &Theme,
) -> Option<ActivityBarHit> {
    // The bar has two halves: a transient LEFT segment (spinner + status +
    // pursuit + elapsed + review alert) shown only while a turn is active,
    // and a persistent RIGHT-pinned todos badge shown whenever a non-empty
    // task list exists — including when idle. If neither half has content,
    // the bar is hidden entirely.
    let status_active = !status.is_empty() && status != "idle";
    let dim = Style::default().fg(theme.muted());

    // ── Build the right-pinned todos badge ──
    // `todos d/t`, always right-aligned so it reads as a persistent status
    // chip distinct from the transient activity on the left.
    let mut todos_rect: Option<Rect> = None;
    let todos_badge: Option<(String, usize)> = todos.filter(|l| !l.items.is_empty()).map(|list| {
        use neenee_core::TodoStatus;
        let done = list.count(TodoStatus::Completed);
        let total = list.items.len();
        let badge = format!("todos {done}/{total}");
        let w = UnicodeWidthStr::width(badge.as_str());
        (badge, w)
    });

    // If there is nothing to show at all (no transient activity and no todos),
    // hide the bar — no point painting a blank row.
    if !status_active && todos_badge.is_none() {
        return None;
    }

    // ── Build the transient left segment ──
    let mut spans: Vec<Span> = Vec::new();
    if status_active {
        let spinner = spinner_glyph();
        let spinner_color = breathing_color(spinner_phase, theme.brand(), theme.surface());

        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            spinner,
            Style::default()
                .fg(spinner_color)
                .add_modifier(Modifier::BOLD),
        ));

        // Lead segment: the live status — the thing that changes frame to
        // frame, so it is the visual focus (brand + italic). The structural
        // counters (turn/round/model) are deliberately absent; they live in
        // the Activity modal that this bar opens on click.
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            status,
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::ITALIC),
        ));

        // Pursuit badge: shown only while a pursuit is armed, as
        // `⟴ <objective>`, so the user can tell at a glance that the turn is
        // part of a larger goal. The objective is truncated to keep the
        // single-line bar compact; the full text is one click away in the
        // Activity modal.
        if let Some(p) = pursuit.filter(|p| !p.is_complete) {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled("⟴ ", dim));
            spans.push(Span::styled(truncate_for_bar(&p.objective, 32), dim));
        }

        // Elapsed: the only live counter on the bar, shown while the turn
        // timer runs. Dropped between turns (no `turn_started_at`).
        if let Some(started) = turn_started_at {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(format_elapsed(started.elapsed()), dim));
        }

        // Session-review alert (ADR-0016): surfaced when a periodic
        // diagnostic judged the turn watch-worthy or stuck. Rendered with
        // the same breathing luminance sweep as the running-indicator dot so
        // the alert pulses gently rather than sitting as a flat warning chip
        // — the motion draws the eye without being frantic. An `Esc to
        // interrupt` hint tells the user they can stop it. Empty alert =
        // clear (nothing rendered).
        if !review_alert.is_empty() {
            let warn = breathing_color(spinner_phase, theme.warning, theme.surface());
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("⚠ {review_alert}"),
                Style::default().fg(warn).add_modifier(Modifier::BOLD),
            ));
        }
    }

    // ── Right-pin the todos badge ──
    if let Some((badge, badge_w)) = todos_badge {
        let left_w: usize = spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let row_w = rect.width as usize;
        // Place the badge flush against the right edge with a 1-cell margin.
        let right_margin = 1;
        let gap = row_w.saturating_sub(left_w + badge_w + right_margin);
        // The badge's absolute column = left_w + gap.
        let badge_col = rect.x + (left_w + gap) as u16;
        if status_active && gap > 0 {
            // Pad between the transient segment and the badge.
            spans.push(Span::raw(" ".repeat(gap)));
        } else if !status_active {
            // Idle: the badge is the only content; push it to the right edge
            // with leading padding rather than leaving it left-aligned.
            spans.push(Span::raw(" ".repeat(gap)));
        }
        spans.push(Span::styled(badge, dim));
        todos_rect = Some(Rect::new(badge_col, rect.y, badge_w as u16, 1));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
    Some(ActivityBarHit {
        bar_rect: rect,
        todos_rect,
    })
}

/// Truncate `s` to at most `max` display cells, appending `…` when cut, so a
/// long pursuit objective does not overflow the single-line activity bar.
fn truncate_for_bar(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
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

    // Windowing: `suggestion_index` is the global index into the full list,
    // but only `MAX_VISIBLE` rows fit on screen. Without a scroll offset the
    // highlight would scroll off the bottom (and the up-arrow wrap path
    // would land on a row that is never rendered). The offset is recomputed
    // every frame from `selected_idx` so it tracks the cursor live:
    //   - when the cursor moves below the visible window, scroll down one
    //     row at a time so it stays on the last visible line;
    //   - when the cursor moves above (e.g. ↑ wraps from 0 to len-1), jump
    //     the window so the cursor sits on the last visible line;
    //   - otherwise leave it alone so short up/down moves inside the window
    //     don't jitter the list.
    let total = completions.len();
    let scroll_offset = match selected_idx {
        // Once the cursor passes the first page (sel >= MAX_VISIBLE), pin it
        // to the last visible row and slide the window up under it — that way
        // every ↓ just brings the next candidate into view at the bottom.
        // For the wrap path (↑ from 0 to len-1), `sel - (MAX_VISIBLE - 1)`
        // also yields the correct bottom-anchored window. Below MAX_VISIBLE,
        // the window stays at the top so short moves don't jitter the list.
        Some(sel) if sel >= MAX_VISIBLE && total > MAX_VISIBLE => {
            (sel - (MAX_VISIBLE - 1)).min(total - MAX_VISIBLE)
        }
        _ => 0,
    };
    let window_end = (scroll_offset + MAX_VISIBLE).min(total);
    let visible_rows = completions[scroll_offset..window_end].to_vec();
    let visible_count = visible_rows.len();
    let popup_height = visible_count as u16;

    // Compute width from content. The description column is dropped entirely
    // (separator + padding) when no candidate carries a description — the
    // `@path` menu uses empty descriptions for a plain list of paths,
    // matching opencode's minimal aesthetic. Width is derived from the full
    // candidate list (not just the visible window) so the popup doesn't
    // resize as the user scrolls.
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
    let popup_width = inner_width + 2; // left + right padding

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

    let block = RtBlock::default().style(Style::default().bg(theme.body()));

    let lines: Vec<Line> = visible_rows
        .iter()
        .enumerate()
        .map(|(row, c)| {
            // `row` is the on-screen position (0..MAX_VISIBLE); recover the
            // global index by adding the scroll offset so the highlight
            // check matches the value passed in `selected_idx`.
            let global_idx = row + scroll_offset;
            let is_selected = Some(global_idx) == selected_idx;
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

/// Inputs for [`draw_hint_bar`]. Carries the model + context-usage info that
/// the old top header showed, now collapsed onto one row.
pub struct HintBarView<'a> {
    pub current_provider: &'a str,
    pub current_model: &'a str,
    pub messages: &'a [TranscriptMessage],
    /// True while the prompt is a `!`-prefixed shell command and no transcript
    /// step is focused. Renders a `[ SHELL ]` pill at the start of the bar in
    /// the warning tone so the user can tell at a glance the next Enter runs
    /// the rest of the line directly through the bash tool — no LLM roundtrip.
    /// When false (and unattended is off) the left of the bar is empty: there
    /// is no compose/browse mode to advertise, the focused-step highlight
    /// indicates navigation.
    pub shell_active: bool,
    /// True while write-tool permission prompts are bypassed this session
    /// (`--unattended` / `/unattended on`). Renders a flat `UNATTENDED` label
    /// in the warning tone at the left edge of the bar so the elevated,
    /// no-prompt state is unmissable without occupying a raised pill — plain
    /// text that carries its meaning without any chrome.
    pub unattended: bool,
}

/// Draw the single-line hint bar pinned below the input box. Carries the model
/// name and context-usage info that the old top header showed, now collapsed
/// onto one row so the transcript reclaims the vertical space.
///
/// Layout: focus-zone pill (shell mode) on the left,
/// right-aligned cluster of `model · context-usage` on the right.
pub fn draw_hint_bar(frame: &mut Frame, rect: Rect, view: HintBarView<'_>, theme: &Theme) {
    let HintBarView {
        current_provider,
        current_model,
        messages,
        shell_active,
        unattended,
    } = view;

    let bg = theme.surface();
    let accent_bg = theme.raised();
    let full_w = rect.width as usize;

    // --- Left cluster: unattended label and/or shell pill.
    //
    // The left edge carries up to two warning-tone signals. `UNATTENDED` is a
    // flat label (no bracket chrome) so it reads as a persistent state flag,
    // while `[ SHELL ]` is a raised pill that advertises an active input
    // mode. When both are on they sit side by side, separated by a segment
    // gap, sharing the warning tone so the left cluster still reads as one
    // warning group.
    let warn_fg = theme.warn();
    let mut zone_spans: Vec<Span<'static>> = Vec::new();

    if unattended {
        zone_spans.push(Span::styled(
            "UNATTENDED",
            Style::default()
                .fg(warn_fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let shell_text = " SHELL ";
    if shell_active {
        if !zone_spans.is_empty() {
            zone_spans.push(Span::styled(
                " ".repeat(HINT_BAR_SEGMENT_GAP),
                Style::default().bg(bg),
            ));
        }
        zone_spans.push(Span::styled(
            "[",
            Style::default().fg(warn_fg).bg(accent_bg),
        ));
        zone_spans.push(Span::styled(
            shell_text,
            Style::default()
                .fg(warn_fg)
                .bg(accent_bg)
                .add_modifier(Modifier::BOLD),
        ));
        zone_spans.push(Span::styled(
            "]",
            Style::default().fg(warn_fg).bg(accent_bg),
        ));
    }

    let zone_pill_width = zone_spans.iter().map(|s| s.content.width()).sum::<usize>();

    // --- Right cluster: model name and context bar.
    // Build each segment separately so we can drop optional ones when the
    // terminal is too narrow.
    let context_max = crate::tui::provider_context_window(current_provider);

    // Left side: optional shell pill. Computed now so the gap to the right
    // cluster can hug the right edge.
    let inner = HINT_BAR_INNER_PADDING;
    let left_used = inner + zone_pill_width;

    // Reserve the right-side segments from the inside out: model is always
    // shown; then context bar. Each preceding segment is separated by
    // `HINT_BAR_SEGMENT_GAP`. The context bar is appended last so we can
    // shrink it to fit the row (see below). We then compute the gap between
    // the left cluster and the right cluster so the cluster hugs the right
    // edge.
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    let mut right_width = 0usize;

    // Model name (always present). Resolve the friendly preset name (e.g.
    // `DeepSeek V4 Pro`) from the provider id so the always-visible indicator
    // matches the `/provider` picker instead of leaking the raw model id
    // (`deepseek-v4-pro`); fall back to the model id for non-preset providers.
    let model_label = crate::tui::model_display_name(current_model);
    right_width += model_label.width();
    right_spans.push(Span::styled(
        model_label,
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD)
            .bg(bg),
    ));

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
    spans.push(Span::styled(" ".repeat(gap), Style::default().bg(bg)));
    spans.extend(right_spans);
    // Trailing fill so the row owns every cell on this line.
    let used = left_used + gap + right_width;
    spans.push(Span::styled(
        " ".repeat(full_w.saturating_sub(used)),
        Style::default().bg(bg),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// Context-usage ratio at which the usage bar turns from green to yellow.
const CONTEXT_USAGE_WARN_THRESHOLD: f64 = 0.7;
/// Context-usage ratio at which the usage bar turns from yellow to red.
const CONTEXT_USAGE_CRIT_THRESHOLD: f64 = 0.9;

/// Compact wall-clock elapsed for the activity bar: `12s`, `1m 23s`,
/// `1h 02m`. Stays short so it fits the single-line activity bar even with a
/// long model name + status. Sub-second durations render as `0s` rather than
/// `0ms` because the bar ticks at most a few times per second and showing
/// millisecond precision would flicker without adding signal. Shared with the
/// Activity modal so the bar and the modal report the same elapsed format.
pub(crate) fn format_elapsed(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        let m = total_secs / 60;
        let s = total_secs % 60;
        format!("{}m {:02}s", m, s)
    } else {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    }
}

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
fn context_usage_spans(used: usize, max: usize, theme: &Theme, bg: Color) -> Vec<Span<'static>> {
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
        Span::styled(format!(" ({}%)", pct), Style::default().fg(color).bg(bg)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The hint bar must render the model and context bar on a single line
    /// below the input without panicking.
    #[test]
    fn hint_bar_renders_model_and_context() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(80, 3);
        let messages = vec![TranscriptMessage::new(neenee_core::Role::User, "hi")];
        terminal.draw(|f| {
            draw_hint_bar(
                f,
                Rect::new(0, 2, 80, 1),
                HintBarView {
                    current_provider: "mock",
                    current_model: "mock-model",
                    messages: &messages,
                    shell_active: false,
                    unattended: false,
                },
                &theme,
            );
        });
    }

    #[test]
    fn hint_bar_pill_shows_shell_only_when_active() {
        let theme = Theme::default();
        let messages: Vec<TranscriptMessage> = vec![];
        // Helper that draws the bar for a given `shell_active` and returns the
        // leading bracketed pill text, or "" when the left side is empty.
        fn pill_text(terminal: &mut neenee_tui::TestTerminal, shell_active: bool) -> String {
            let mut captured = String::new();
            terminal.draw(|f| {
                let view = HintBarView {
                    current_provider: "",
                    current_model: "",
                    messages: &Vec::<TranscriptMessage>::new(),
                    shell_active,
                    unattended: false,
                };
                draw_hint_bar(f, Rect::new(0, 0, 80, 1), view, &Theme::default());
            });
            let buf = terminal.buffer();
            let bw = buf.area().width as usize;
            for x in 0..bw {
                let cell = &buf.content[x];
                captured.push_str(cell.symbol());
            }
            let trimmed = captured.trim_start();
            // Only a `[…]` pill that begins the row counts; otherwise the left
            // side is empty (the model label lives on the right).
            if trimmed.starts_with('[') {
                let end = trimmed.find(']').map(|i| i + 1).unwrap_or(trimmed.len());
                trimmed[..end].trim().to_string()
            } else {
                String::new()
            }
        }

        let mut terminal = neenee_tui::TestTerminal::new(80, 1);
        let _ = &messages;

        // No `!` typed → no left pill at all (no compose/browse mode badge).
        assert_eq!(pill_text(&mut terminal, false), "");
        // `!`-prefixed input → SHELL pill.
        assert_eq!(pill_text(&mut terminal, true), "[ SHELL ]");
        let _ = theme;
    }
}
