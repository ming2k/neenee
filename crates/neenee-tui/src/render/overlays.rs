//! Overlay modal renderers: model picker, sessions, history search, the
//! permission sheet, API-key / endpoint / model-name prompts, the help overlay,
//! and the copy / armed toasts. Plus the relative-time formatter used by the
//! sessions list.

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::layout::LayoutMap;
use neenee_core::PermissionRequest;

use super::primitives::{
    centered_rect, contrast_fg, draw_dim_backdrop, panel_block, viewport_rect,
};
use super::Theme;

// The permission sheet renders inline, replacing the composer (input box)
// area. Collapsed it shows a one-line summary plus the action footer;
// expanding "Details" grows the body upward into the transcript.
const PERMISSION_H_PADDING: u16 = 2;
const PERMISSION_TOP_PADDING: u16 = 1;
const PERMISSION_FOOTER_HEIGHT: u16 = 1;
const PERMISSION_BODY_FOOTER_GAP: u16 = 1;
/// Max body rows in the collapsed (summary-only) state.
const PERMISSION_COLLAPSED_BODY_CAP: u16 = 2;
/// Max body rows when "Details" is expanded; the rest is scrollable.
const PERMISSION_MAX_BODY_ROWS: u16 = 14;

/// Draw the models modal. `key_status` maps lowercase provider names to
/// whether a usable API key is available (env or config).
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    solutions: &[crate::ModelSolution],
    current_provider: &str,
    modal_index: usize,
    key_status: &HashMap<String, bool>,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(72, 60, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(vec![Span::styled(
        " Select Model Solution",
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )])];

    for (i, solution) in solutions.iter().enumerate() {
        let is_current = solution.id == current_provider;
        let is_selected = i == modal_index;
        let row_bg = if is_selected {
            theme.brand()
        } else {
            theme.panel()
        };
        let row_fg = if is_selected {
            contrast_fg(theme.brand())
        } else {
            theme.fg()
        };
        let base = Style::default().bg(row_bg).fg(row_fg);
        let (key_label, key_color) = match key_status.get(solution.id) {
            Some(true) => ("✓ ready", theme.ok()),
            Some(false) => ("✗ no key", theme.err()),
            None => ("", row_fg),
        };
        let key_style = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.brand()))
        } else {
            Style::default().fg(key_color)
        };
        let prefix = if is_current { "● " } else { "  " };
        let dim = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.brand()))
        } else {
            Style::default().fg(theme.muted())
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", prefix), dim),
            Span::styled(
                format!("{:<14} ", solution.name),
                base.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:<9} ", key_label), key_style),
            Span::styled(format!("{} ", solution.model), dim),
            Span::styled(format!("· {}", solution.description), dim),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter select/setup · k configure · Esc ",
        Style::default().fg(theme.muted()),
    )));

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn draw_solution_input_modal(
    frame: &mut Frame,
    title: &str,
    help: &str,
    value: &str,
    masked: bool,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(60, 30, viewport_rect(frame));
    frame.render_widget(Clear, area);
    let display = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", title),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" {}", help),
            Style::default().fg(theme.muted()),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.brand())),
            Span::styled(display, Style::default().fg(theme.fg())),
            Span::styled("▏", Style::default().fg(theme.brand())),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Enter continue · Esc cancel ",
            Style::default().fg(theme.muted()),
        )),
    ];

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the API-key entry modal. The key itself is already masked by the caller.
pub fn draw_api_key_modal(frame: &mut Frame, provider: &str, masked_key: &str, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(56, 34, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" API key · {}", provider),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Key  ", Style::default().fg(theme.muted())),
            Span::styled(
                masked_key.to_string(),
                Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(theme.brand())),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Saved to the local config file; environment",
            Style::default().fg(theme.muted()),
        )),
        Line::from(Span::styled(
            " variables of the same provider still win.",
            Style::default().fg(theme.muted()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Enter save & switch · Esc cancel ",
            Style::default().fg(theme.muted()),
        )),
    ];

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render a unix timestamp as a short relative time (e.g. "2h ago", "3d ago").
pub fn relative_time(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(ts);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 7 * 86_400 {
        format!("{}d ago", diff / 86_400)
    } else {
        format!("{}w ago", diff / (7 * 86_400))
    }
}

/// Draw the sessions picker: each row shows the session overview plus its
/// creation and last-interaction times. Enter opens the selected session.
pub fn draw_sessions_modal(
    frame: &mut Frame,
    sessions: &[neenee_core::SessionOverview],
    selected: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(80, 64, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Sessions",
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    ))];

    if sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " No previous sessions yet.",
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
        let overview: String = session.overview.chars().take(48).collect();
        let meta = format!(
            "{} msgs · created {} · active {}",
            session.message_count,
            relative_time(session.created_at),
            relative_time(session.updated_at)
        );
        let overview_used = 1 + badge.len() + overview.width();
        let meta_used = 2 + meta.width();
        // Right-align the meta on the same row when it fits.
        let inner_width = area.width.saturating_sub(2) as usize;
        let gap = inner_width.saturating_sub(overview_used.min(inner_width / 2) + meta_used);
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}{}", badge, overview),
                Style::default().bg(bg).fg(fg),
            ),
            Span::styled(" ".repeat(gap), Style::default().bg(bg)),
            Span::styled(format!("  {}  ", meta), Style::default().bg(bg).fg(muted)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter open · d delete · Esc close ",
        Style::default().fg(theme.muted()),
    )));

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the history search modal.
///
/// `query` is the fuzzy query the user is typing into the (borrowed) input
/// box; `ranked` is the pre-computed `(original_history_index, FuzzyMatch)`
/// list produced by [`crate::App::history_filtered`] — passing it in avoids a
/// second fuzzy pass per frame. `modal_index` selects into `ranked`.
///
/// Each result line highlights the matched characters of the query so the
/// user can see why an entry surfaced. Empty query → show everything with no
/// highlights; query with no matches → "no matches" placeholder.
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    query: &str,
    ranked: &[(usize, crate::fuzzy::FuzzyMatch)],
    modal_index: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(70, 55, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(
            " Input History",
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("❯ ", Style::default().fg(theme.muted())),
        Span::styled(
            if query.is_empty() {
                "type to fuzzy-filter"
            } else {
                query
            },
            Style::default()
                .fg(if query.is_empty() {
                    theme.muted()
                } else {
                    theme.fg()
                })
                .add_modifier(Modifier::BOLD),
        ),
    ])];

    if history.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  (no history yet — send a message to populate this list)",
            Style::default().fg(theme.muted()),
        )));
    } else if ranked.is_empty() {
        // Query produced no matches.
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )));
    } else {
        // Build a char-index set per row for O(1) "is this char matched?"
        // checks during rendering.
        for (row, (orig_idx, m)) in ranked.iter().enumerate() {
            let is_selected = row == modal_index;
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
            let num_style = if is_selected {
                Style::default().bg(bg).fg(contrast_fg(theme.brand()))
            } else {
                Style::default().fg(theme.muted())
            };
            let base_style = Style::default().bg(bg).fg(fg);
            let matched_style = if is_selected {
                // On the highlighted row, keep the match underline but stay
                // readable on the primary-color background.
                Style::default()
                    .bg(bg)
                    .fg(contrast_fg(theme.brand()))
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
                    .bg(bg)
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD)
            };

            let entry = history.get(*orig_idx).map(String::as_str).unwrap_or("");
            let mut spans: Vec<Span> = Vec::with_capacity(entry.chars().count() + 1);
            spans.push(Span::styled(format!(" {:>3} ", row + 1), num_style));
            let matched: std::collections::HashSet<usize> = m.positions.iter().copied().collect();
            for (char_idx, c) in entry.chars().enumerate() {
                let style = if matched.contains(&char_idx) {
                    matched_style
                } else {
                    base_style
                };
                spans.push(Span::styled(c.to_string(), style));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " type to filter · ↑↓ navigate · Enter insert · Esc close ",
        Style::default().fg(theme.muted()),
    )));

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a blocking tool permission request inline, replacing the composer
/// (input box) area. The transcript above stays visible and scrollable.
///
/// Collapsed (the default) the sheet is a one-line summary — the tool name
/// plus its scope (the specific path/command being touched) — followed by a
/// footer of inline actions. Selecting "Details" expands the body upward to
/// reveal the full description and arguments without leaving the prompt.
pub fn draw_permission_sheet(
    frame: &mut Frame,
    request: &PermissionRequest,
    selected: usize,
    confirm_always: bool,
    show_details: bool,
    scroll: usize,
    input_rect: Rect,
    theme: &Theme,
) -> usize {
    let area_bottom = input_rect.y + input_rect.height;

    let arguments = serde_json::from_str::<serde_json::Value>(&request.arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| request.arguments.clone());
    let scope_meaningful = !request.scope.is_empty() && request.scope != "*";

    // Header line: tool name, plus the concrete scope (path/command) when
    // it adds information. The scope is the single most useful detail, so it
    // earns a spot in the collapsed summary; everything else hides behind
    // "Details". The left bar carries the severity cue.
    let mut header = vec![Span::styled(
        request.tool.clone(),
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )];
    if confirm_always {
        header.push(Span::styled(
            " — always allow until exit?",
            Style::default().fg(theme.fg()),
        ));
    } else if scope_meaningful {
        header.push(Span::styled("  ", Style::default()));
        header.push(Span::styled(
            request.scope.clone(),
            Style::default().fg(theme.info()),
        ));
    }

    let mut body_lines: Vec<Line> = Vec::new();
    body_lines.push(Line::from(header));

    if confirm_always {
        body_lines.push(Line::from(Span::styled(
            "Grants this tool until neenee exits.",
            Style::default().fg(theme.muted()),
        )));
    } else if show_details {
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            request.description.clone(),
            Style::default().fg(theme.fg()),
        )));
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            "Arguments",
            Style::default()
                .fg(theme.info())
                .add_modifier(Modifier::BOLD),
        )));
        body_lines.extend(arguments.lines().map(|line| {
            Line::from(line).style(Style::default().fg(theme.code_text()))
        }));
    }

    let fixed =
        PERMISSION_TOP_PADDING + PERMISSION_BODY_FOOTER_GAP + PERMISSION_FOOTER_HEIGHT;
    let content_w = input_rect
        .width
        .saturating_sub(1 + 2 * PERMISSION_H_PADDING)
        .max(1);
    let body_total_rows: usize = body_lines
        .iter()
        .map(|line| {
            let width: usize = line.spans.iter().map(|span| span.content.width()).sum();
            width.max(1).div_ceil(content_w as usize)
        })
        .sum();

    // How tall the body may grow. Collapsed stays compact; expanded climbs
    // into the transcript but never past the top of the viewport.
    let body_cap: u16 = if confirm_always {
        body_total_rows.min(2).min(u16::MAX as usize) as u16
    } else if show_details {
        let room = area_bottom.saturating_sub(fixed).max(1);
        PERMISSION_MAX_BODY_ROWS.min(room)
    } else {
        PERMISSION_COLLAPSED_BODY_CAP
    };
    let body_h = (body_total_rows as u16).min(body_cap);
    let max_scroll = body_total_rows.saturating_sub(body_h as usize);
    let body_scroll = scroll.min(max_scroll);

    let desired_h = fixed + body_h;
    // Fill the composer slot when collapsed (so it reads as a drop-in
    // replacement for the input box); grow above it when expanded.
    let sheet_h = desired_h
        .max(input_rect.height)
        .min(area_bottom)
        .max(1);
    let sheet_top = area_bottom.saturating_sub(sheet_h);
    let area = Rect::new(input_rect.x, sheet_top, input_rect.width, sheet_h);

    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(theme.warn(), theme.panel()), area);

    let content_x = area.x + 1 + PERMISSION_H_PADDING;
    let body_area = Rect::new(
        content_x,
        area.y + PERMISSION_TOP_PADDING,
        content_w,
        body_h,
    );
    frame.render_widget(
        Paragraph::new(body_lines)
            .scroll((body_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        body_area,
    );

    let footer_y = area
        .y
        .saturating_add(sheet_h)
        .saturating_sub(PERMISSION_FOOTER_HEIGHT);
    let footer_band = Rect::new(
        area.x + 1,
        footer_y,
        area.width.saturating_sub(1),
        PERMISSION_FOOTER_HEIGHT,
    );
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.raised())),
        footer_band,
    );

    let details_label = if show_details { "Hide" } else { "Details" };
    let labels: Vec<&str> = if confirm_always {
        vec!["Confirm always", "Cancel"]
    } else {
        vec!["Allow once", "Always allow", "Reject", details_label]
    };

    let mut footer_spans: Vec<Span> = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let is_cancel = confirm_always && index == 1;
        let is_reject = !confirm_always && index == 2;
        let is_selected = index == selected;
        let bg = if is_selected {
            if is_reject || is_cancel {
                theme.err()
            } else {
                theme.brand()
            }
        } else {
            theme.raised()
        };
        let fg = if is_selected {
            contrast_fg(bg)
        } else {
            theme.fg()
        };
        if index > 0 {
            footer_spans.push(Span::styled("  ", Style::default().bg(theme.raised())));
        }
        footer_spans.push(Span::styled(
            format!(" {} ", label),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
    }
    let hint = if confirm_always {
        " ←→ select · Enter confirm · Esc back "
    } else if max_scroll > 0 {
        " ↑↓ scroll details · ←→ select · Enter · Esc reject "
    } else {
        " ←→ select · Enter · Esc reject "
    };
    let hint_width = hint.width();
    let footer_width = content_w as usize;
    let used: usize = footer_spans.iter().map(|s| s.content.width()).sum();
    if used + hint_width <= footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used - hint_width),
            Style::default().bg(theme.raised()),
        ));
        footer_spans.push(Span::styled(
            hint,
            Style::default().bg(theme.raised()).fg(theme.muted()),
        ));
    } else if used < footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used),
            Style::default().bg(theme.raised()),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(footer_spans)),
        Rect::new(content_x, footer_y, content_w, PERMISSION_FOOTER_HEIGHT),
    );
    max_scroll
}

/// Draw an armed-action toast (e.g. "press Ctrl+C again to exit",
/// "press Esc again to interrupt"). Warn-colored like the original exit toast.
pub fn draw_armed_toast(frame: &mut Frame, message: &str, theme: &Theme) {
    let size = frame.size();
    toast(frame, theme, message, theme.warn(), size.width);
}

/// Draw the help / keybindings modal.
/// Full-output detail overlay for a focused tool step (ADR-0001 Step 8). Shows
/// the step's complete output in a centered, scrollable panel so a long result
/// can be inspected without scrolling the whole transcript. Shell output is
/// broken into `$ command`, stdout, stderr (in `error_fg`), and an exit footer
/// straight from the structured payload — no string-sniffing.
pub fn draw_tool_step_detail_overlay(
    frame: &mut Frame,
    msg: &crate::document::TranscriptMessage,
    scroll: u16,
    theme: &Theme,
) {
    use crate::document::MessageKind;
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(92, 84, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(summary) = msg.tool_step_summary() {
        lines.push(Line::from(Span::styled(
            summary,
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    let body_style = Style::default().fg(theme.fg());
    let stderr_style = Style::default().fg(theme.err());
    let marker_style = Style::default().fg(theme.warn()).add_modifier(Modifier::BOLD);
    match &msg.kind {
        MessageKind::ToolStep {
            structured:
                Some(neenee_core::ToolOutput::Shell {
                    command,
                    stdout,
                    stderr,
                    exit,
                    truncated,
                }),
            ..
        } => {
            if !command.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("$ {}", command),
                    Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
                )));
            }
            for line in stdout.trim_end_matches(&['\r', '\n'][..]).split('\n') {
                lines.push(Line::from(Span::styled(line.to_string(), body_style)));
            }
            if !stderr.is_empty() {
                for line in stderr.trim_end_matches(&['\r', '\n'][..]).split('\n') {
                    lines.push(Line::from(Span::styled(line.to_string(), stderr_style)));
                }
            }
            if *truncated {
                lines.push(Line::from(Span::styled(
                    "[output truncated]".to_string(),
                    marker_style,
                )));
            }
            if matches!(exit, Some(c) if *c != 0) {
                lines.push(Line::from(Span::styled(
                    format!("exit {}", exit.unwrap()),
                    marker_style,
                )));
            }
        }
        MessageKind::ToolStep {
            output: Some(output),
            ..
        } => {
            for line in output.split('\n') {
                lines.push(Line::from(Span::styled(line.to_string(), body_style)));
            }
        }
        _ => {}
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑/↓ or wheel scroll · esc close ",
        Style::default().fg(theme.muted()),
    )));

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(block),
        area,
    );
}

pub fn draw_help_modal(frame: &mut Frame, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(58, 70, viewport_rect(frame));
    frame.render_widget(Clear, area);

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

    let lines = vec![
        Line::from(Span::styled(
            " Help",
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("alt+enter", "insert newline (ctrl+j)"),
        row("esc", "interrupt (×2) / close"),
        row("ctrl+c", "copy · interrupt · quit (×2)"),
        Line::from(""),
        Line::from(section("Focus zones")),
        Line::from(desc(
            "Two zones split the keyboard. The hint bar shows [COMPOSE] or",
        )),
        Line::from(desc(
            "[BROWSE] so you always know where the next key lands.",
        )),
        row("tab", "toggle compose ↔ browse"),
        row("shift+tab", "toggle (browse: last step)"),
        row("↑ / ↓", "compose: history · browse: cycle steps"),
        row("esc / any key", "browse → compose"),
        row("enter / ␠", "browse: activate focused step"),
        Line::from(""),
        Line::from(section("Views & tools")),
        row("ctrl+h", "this help"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("/", "slash commands"),
        Line::from(""),
        Line::from(section("Modes")),
        row("/mode", "build · plan"),
        row("/goal", "set or manage the goal"),
        row("/loop N", "bounded autonomous work"),
        Line::from(""),
        Line::from(desc("Drag to select · Ctrl+C or Ctrl+Shift+C to copy.")),
        Line::from(""),
        Line::from(Span::styled(
            " esc · close ",
            Style::default().fg(theme.muted()),
        )),
    ];

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a "copied to clipboard" toast.
pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.size();
    let color = if failed {
        theme.err()
    } else {
        theme.ok()
    };
    toast(frame, theme, message, color, size.width);
}

/// opencode-style toast: top-right panel with variant-colored left/right bars.
fn toast(frame: &mut Frame, theme: &Theme, message: &str, color: Color, width: u16) {
    let text = format!(" {} ", message.trim());
    // Inner width (text) capped, plus the two border columns.
    let inner_w = text.width() as u16;
    let toast_width = inner_w.min(58) + 2;
    let x = width.saturating_sub(toast_width).saturating_sub(2).max(1);
    let area = Rect::new(x, 1, toast_width, 3);

    let block = RtBlock::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel()));

    let line = Line::from(Span::styled(
        text,
        Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
    ));
    // Vertically center the single line within the 3-row panel.
    let para = Paragraph::new(vec![Line::from(""), line]);
    frame.render_widget(para.block(block), area);
}
