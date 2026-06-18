//! Transient chrome around the input box: the activity status bar with an
//! animated braille spinner, the right-aligned keybinding hint line, and the
//! slash-command suggestion popup anchored above the input.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::layout::LayoutMap;

use super::util::{contrast_fg, viewport_rect};
use super::Theme;

/// Braille spinner frames used by the transient status bar above the input
/// box. Cycling through these on each frame gives a clear sense of motion
/// (10 frames ≈ one revolution per second at the 100ms tick rate).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner_frame(spinner_phase: usize) -> &'static str {
    SPINNER_FRAMES[spinner_phase % SPINNER_FRAMES.len()]
}

/// Draw the transient activity bar that sits directly above the input box.
/// Replaces the old inline `┃ neenee ⟳ <status>` indicator: the brand prefix
/// is dropped (the header already shows it) and the static `⟳` glyph is
/// replaced by an animated braille spinner so the harness never looks frozen.
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
    let spinner = spinner_frame(spinner_phase);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            spinner,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            status,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::ITALIC),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

/// Draw the right-aligned hint line below the input box. Each entry is
/// `key description`, with the key highlighted and the description muted.
pub fn draw_hint(frame: &mut Frame, rect: Rect, hints: &[(&str, &str)], theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    let entries: Vec<String> = hints
        .iter()
        .map(|(key, desc)| format!("{} {}", key, desc))
        .collect();
    let total: usize = entries.iter().map(|s| s.width()).sum::<usize>() + (entries.len() * 3);
    let width = rect.width as usize;
    let lead = total.min(width);
    if lead < width {
        spans.push(Span::raw(" ".repeat(width - lead)));
    }
    for (i, ((key, _), text)) in hints.iter().zip(entries.iter()).enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(theme.text_muted)));
        }
        spans.push(Span::styled(
            format!("{} ", key),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
        let desc_start = key.width() + 1;
        if desc_start < text.len() {
            spans.push(Span::styled(
                &text[desc_start..],
                Style::default().fg(theme.text_muted),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// Draw a slash-command suggestion popup anchored above the input box.
pub fn draw_suggestions(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    suggestions: &[(&str, &str)],
    selected_idx: Option<usize>,
    anchor: Rect,
    theme: &Theme,
) {
    if suggestions.is_empty() {
        return;
    }

    const MAX_VISIBLE: usize = 6;

    let visible_count = suggestions.len().min(MAX_VISIBLE);
    // +1 line for the bottom hint bar.
    let popup_height = visible_count as u16 + 2;

    // Compute width from content.
    let max_cmd = suggestions
        .iter()
        .map(|(c, _)| c.width())
        .max()
        .unwrap_or(0);
    let max_desc = suggestions
        .iter()
        .map(|(_, d)| d.width())
        .max()
        .unwrap_or(0);
    let inner_width = (max_cmd + 3 + max_desc).max(30) as u16;
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

    let more_hint = if suggestions.len() > MAX_VISIBLE {
        format!(" … +{} more ", suggestions.len() - MAX_VISIBLE)
    } else {
        " ".to_string()
    };

    let block = RtBlock::default()
        .borders(Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(theme.primary))
        .style(Style::default().bg(theme.menu_bg));

    // Custom list: render the block, then items inside, plus a title line.
    let title = Line::from(vec![Span::styled(
        " Commands",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )]);

    let mut lines: Vec<Line> = vec![title];
    lines.extend(
        suggestions
            .iter()
            .take(MAX_VISIBLE)
            .enumerate()
            .map(|(i, (cmd, desc))| {
                let is_selected = Some(i) == selected_idx;
                let style = if is_selected {
                    Style::default()
                        .bg(theme.primary)
                        .fg(contrast_fg(theme.primary))
                } else {
                    Style::default().fg(theme.text)
                };
                let cmd_style = if is_selected {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
                };
                Line::from(vec![
                    Span::styled(" ", Style::default()),
                    Span::styled(format!("{:<width$} ", cmd, width = max_cmd), cmd_style),
                    Span::styled("· ", Style::default().fg(theme.text_muted)),
                    Span::styled(
                        *desc,
                        if is_selected {
                            style
                        } else {
                            Style::default().fg(theme.text_muted)
                        },
                    ),
                ])
            }),
    );
    lines.push(Line::from(Span::styled(
        format!("{}↑↓ navigate · Enter select · Esc close", more_hint),
        Style::default().fg(theme.text_muted),
    )));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}
