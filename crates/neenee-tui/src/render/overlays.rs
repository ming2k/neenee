//! Overlay modal renderers: model picker, sessions, history search, the
//! permission sheet, API-key / endpoint / model-name prompts, the help card,
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
use super::{Theme, CHAT_H_INSET};

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
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(72, 60, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(vec![Span::styled(
        " Select Model Solution",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )])];

    for (i, solution) in solutions.iter().enumerate() {
        let is_current = solution.id == current_provider;
        let is_selected = i == modal_index;
        let row_bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let row_fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let base = Style::default().bg(row_bg).fg(row_fg);
        let (key_label, key_color) = match key_status.get(solution.id) {
            Some(true) => ("✓ ready", theme.success),
            Some(false) => ("✗ no key", theme.error_fg),
            None => ("", row_fg),
        };
        let key_style = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(key_color)
        };
        let prefix = if is_current { "● " } else { "  " };
        let dim = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(theme.text_muted)
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
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
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
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
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
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" {}", help),
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.primary)),
            Span::styled(display, Style::default().fg(theme.text)),
            Span::styled("▏", Style::default().fg(theme.primary)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Enter continue · Esc cancel ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the API-key entry modal. The key itself is already masked by the caller.
pub fn draw_api_key_modal(frame: &mut Frame, provider: &str, masked_key: &str, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(56, 34, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" API key · {}", provider),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Key  ", Style::default().fg(theme.text_muted)),
            Span::styled(
                masked_key.to_string(),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(theme.primary)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Saved to the local config file; environment",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(Span::styled(
            " variables of the same provider still win.",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Enter save & switch · Esc cancel ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
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
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(80, 64, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Sessions",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))];

    if sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " No previous sessions yet.",
            Style::default().fg(theme.text_muted),
        )));
    }

    for (i, session) in sessions.iter().enumerate() {
        let is_selected = i == selected;
        let bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let muted = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text_muted
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
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the history search modal.
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    modal_index: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(70, 55, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Chat History",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))];

    for (i, h) in history.iter().enumerate() {
        let is_selected = i == modal_index;
        let bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let num_style = if is_selected {
            Style::default().bg(bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(theme.text_muted)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>3} ", i + 1), num_style),
            Span::styled(h, Style::default().bg(bg).fg(fg)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter insert · Esc close ",
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
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
        Span::styled("△ ", Style::default().fg(theme.warning)),
        Span::styled(
            "Permission required",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(vec![
        Span::styled("Tool ", Style::default().fg(theme.text_muted)),
        Span::styled(
            request.tool.clone(),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Scope ", Style::default().fg(theme.text_muted)),
        Span::styled(request.scope.clone(), Style::default().fg(theme.info)),
    ]));
    body_lines.push(Line::from(Span::styled(
        request.description.clone(),
        Style::default().fg(theme.text),
    )));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(Span::styled(
        "Arguments",
        Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
    )));
    body_lines.extend(
        arg_lines
            .into_iter()
            .map(|line| line.style(Style::default().fg(theme.code_fg))),
    );
    if confirm_always {
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            "This permits the tool until neenee exits.",
            Style::default().fg(theme.warning),
        )));
    }

    let available_width = size.width.saturating_sub(2 * CHAT_H_INSET).max(1);
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

    draw_dim_backdrop(frame, size, theme.backdrop);

    let area = Rect::new(sheet_x, size.y + sheet_top, sheet_width, sheet_h);
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(theme.warning, theme.panel_bg), area);

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
        RtBlock::default().style(Style::default().bg(theme.element_bg)),
        footer_band,
    );

    let mut footer_spans: Vec<Span> = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let is_reject = !confirm_always && index == 2;
        let is_selected = index == selected;
        let bg = if is_selected {
            if is_reject {
                theme.error_fg
            } else {
                theme.warning
            }
        } else {
            theme.element_bg
        };
        let fg = if is_selected {
            contrast_fg(bg)
        } else {
            theme.text
        };
        if index > 0 {
            footer_spans.push(Span::styled("  ", Style::default().bg(theme.element_bg)));
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
            Style::default().bg(theme.element_bg),
        ));
        footer_spans.push(Span::styled(
            hint,
            Style::default().bg(theme.element_bg).fg(theme.text_muted),
        ));
    } else if used < footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used),
            Style::default().bg(theme.element_bg),
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
    toast(frame, theme, message, theme.warning, size.width);
}

/// Draw the help / keybindings modal.
pub fn draw_help_modal(frame: &mut Frame, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(58, 70, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let key = |k: &str| {
        Span::styled(
            format!("{:<10}", k),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &str| Span::styled(d.to_string(), Style::default().fg(theme.text_muted));
    let section = |title: &str| {
        Span::styled(
            title.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )
    };
    let row = |k: &str, d: &str| Line::from(vec![key(k), desc(d)]);

    let lines = vec![
        Line::from(Span::styled(
            " Help",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("alt+enter", "insert newline (ctrl+j)"),
        row("esc", "interrupt (×2) / close"),
        row("ctrl+c", "copy · interrupt · quit (×2)"),
        row("↑ / ↓", "history · navigate"),
        row("tab", "accept suggestion"),
        Line::from(""),
        Line::from(section("Views & tools")),
        row("ctrl+h", "this help"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("ctrl+b", "toggle sidebar (plans)"),
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
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a "copied to clipboard" toast.
pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.size();
    let color = if failed {
        theme.error_fg
    } else {
        theme.success
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
        .style(Style::default().bg(theme.panel_bg));

    let line = Line::from(Span::styled(
        text,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ));
    // Vertically center the single line within the 3-row panel.
    let para = Paragraph::new(vec![Line::from(""), line]);
    frame.render_widget(para.block(block), area);
}
