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
use super::{Theme, TRANSCRIPT_H_INSET};

const PERMISSION_SHEET_MAX_WIDTH: u16 = 118;
const PERMISSION_SHEET_MIN_WIDTH: u16 = 64;
const PERMISSION_SHEET_WIDTH_PERCENT: u16 = 72;
const PERMISSION_SHEET_H_PADDING: u16 = 3;
const PERMISSION_SHEET_TOP_PADDING: u16 = 1;
const PERMISSION_SHEET_BOTTOM_PADDING: u16 = 1;
const PERMISSION_SHEET_INPUT_GAP: u16 = 1;
const PERMISSION_SHEET_MAX_HEIGHT: u16 = 18;

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

/// Draw a blocking tool permission request as a bottom-anchored sheet
/// (opencode-style): dimmed backdrop above, a panel with a warning-colored
/// left bar, the tool/scope/arguments detail, and a footer bar of inline
/// options where the selected one is highlighted.
pub fn draw_permission_sheet(
    frame: &mut Frame,
    request: &PermissionRequest,
    selected: usize,
    confirm_always: bool,
    scroll: usize,
    theme: &Theme,
) -> usize {
    let size = viewport_rect(frame);
    let bottom = size.height;

    let arguments = serde_json::from_str::<serde_json::Value>(&request.arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| request.arguments.clone());
    let arg_lines: Vec<Line> = arguments.lines().map(Line::from).collect();

    let labels: &[&str] = if confirm_always {
        &["Confirm always", "Cancel"]
    } else {
        &["Allow once", "Always allow", "Reject"]
    };

    let mut body_lines: Vec<Line> = Vec::new();
    body_lines.push(Line::from(vec![
        Span::styled("△ ", Style::default().fg(theme.warn())),
        Span::styled(
            "Permission required",
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        ),
    ]));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(vec![
        Span::styled("Tool ", Style::default().fg(theme.muted())),
        Span::styled(
            request.tool.clone(),
            Style::default()
                .fg(theme.warn())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Scope ", Style::default().fg(theme.muted())),
        Span::styled(request.scope.clone(), Style::default().fg(theme.info())),
    ]));
    body_lines.push(Line::from(Span::styled(
        request.description.clone(),
        Style::default().fg(theme.fg()),
    )));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(Span::styled(
        "Arguments",
        Style::default().fg(theme.info()).add_modifier(Modifier::BOLD),
    )));
    body_lines.extend(
        arg_lines
            .into_iter()
            .map(|line| line.style(Style::default().fg(theme.code_text()))),
    );
    if confirm_always {
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            "This permits the tool until neenee exits.",
            Style::default().fg(theme.warn()),
        )));
    }

    let available_width = size.width.saturating_sub(2 * TRANSCRIPT_H_INSET).max(1);
    let preferred_width = available_width.saturating_mul(PERMISSION_SHEET_WIDTH_PERCENT) / 100;
    let min_width = PERMISSION_SHEET_MIN_WIDTH.min(available_width);
    let sheet_width = preferred_width
        .max(min_width)
        .min(PERMISSION_SHEET_MAX_WIDTH)
        .min(available_width)
        .max(1);
    let sheet_x = size.x + size.width.saturating_sub(sheet_width) / 2;

    let input_gap = PERMISSION_SHEET_INPUT_GAP.min(bottom);
    let sheet_bottom = bottom.saturating_sub(input_gap);
    let footer_height: u16 = 1;
    let footer_gap: u16 = 1;
    let fixed_height =
        PERMISSION_SHEET_TOP_PADDING + footer_gap + footer_height + PERMISSION_SHEET_BOTTOM_PADDING;
    let max_h = PERMISSION_SHEET_MAX_HEIGHT.min(sheet_bottom).max(1);
    let content_w = sheet_width
        .saturating_sub(1 + 2 * PERMISSION_SHEET_H_PADDING)
        .max(1);
    let body_total_rows: usize = body_lines
        .iter()
        .map(|line| {
            let width: usize = line.spans.iter().map(|span| span.content.width()).sum();
            width.max(1).div_ceil(content_w as usize)
        })
        .sum();
    let body_capacity = max_h.saturating_sub(fixed_height).max(1);
    let body_h = (body_total_rows as u16).min(body_capacity);
    let max_scroll = body_total_rows.saturating_sub(body_h as usize);
    let body_scroll = scroll.min(max_scroll);
    let sheet_h = (fixed_height + body_h).min(sheet_bottom).max(1);
    let sheet_top = sheet_bottom.saturating_sub(sheet_h);

    draw_dim_backdrop(frame, size, theme.backdrop());

    let area = Rect::new(sheet_x, size.y + sheet_top, sheet_width, sheet_h);
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(theme.warn(), theme.panel()), area);

    let content_x = area.x + 1 + PERMISSION_SHEET_H_PADDING;
    let body_area = Rect::new(
        content_x,
        area.y + PERMISSION_SHEET_TOP_PADDING,
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
        .saturating_sub(PERMISSION_SHEET_BOTTOM_PADDING + footer_height);
    let footer_band = Rect::new(area.x + 1, footer_y, area.width.saturating_sub(1), 1);
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.raised())),
        footer_band,
    );

    let mut footer_spans: Vec<Span> = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let is_reject = !confirm_always && index == 2;
        let is_selected = index == selected;
        let bg = if is_selected {
            if is_reject {
                theme.err()
            } else {
                theme.warn()
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
    let hint = if max_scroll > 0 {
        " ↑↓/PgUp/PgDn scroll · ←→ select · Enter confirm · Esc reject "
    } else {
        " ←→ select · Enter confirm · Esc reject "
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
        Rect::new(content_x, footer_y, content_w, 1),
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
    if let Some(header) = msg.tool_step_header() {
        lines.push(Line::from(Span::styled(
            header,
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
            for line in stdout.split('\n') {
                lines.push(Line::from(Span::styled(line.to_string(), body_style)));
            }
            if !stderr.is_empty() {
                for line in stderr.split('\n') {
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
