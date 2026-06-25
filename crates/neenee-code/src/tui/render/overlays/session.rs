//! Sessions picker and session-context tabbed modal.

use std::collections::HashMap;

use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::common::{
    McpRow, placeholder, relative_time_compact, selectable_row, truncate_ellipsis,
};
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    centered_rect, contrast_fg, modal_frame, render_body, viewport_rect,
};

/// Draw the sessions picker: each row shows the session overview plus its
/// creation and last-interaction times. Enter opens the selected session.
pub fn draw_sessions_modal(
    frame: &mut Frame,
    sessions: &[neenee_core::SessionOverview],
    selected: usize,
    theme: &Theme,
) {
    let area = centered_rect(80, 64, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Sessions",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let body_width = f.body.width as usize;
    let mut body: Vec<Line> = Vec::new();

    if sessions.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            "No previous sessions yet.",
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
        // Drop the message count (low signal) and use compact relative times
        // (no "ago" suffix) so the meta column stays narrow and predictable.
        let meta = format!(
            "created {} · active {}",
            relative_time_compact(session.created_at),
            relative_time_compact(session.updated_at),
        );
        let meta_w = meta.width();
        // Guarantee a fixed gutter between the two columns by giving the
        // overview a width budget of `body_width - meta_w - gutter`, then
        // truncating it with an ellipsis when it overflows. That way a long
        // overview never crowds the meta column, and the gutter is constant
        // row-to-row instead of whatever slack is left over.
        const COL_GUTTER: usize = 2;
        let badge_w = badge.width();
        let col1_budget = body_width.saturating_sub(meta_w + COL_GUTTER);
        let overview = truncate_ellipsis(&session.overview, col1_budget.saturating_sub(badge_w));
        let left = format!("{}{}", badge, overview);
        let left_w = left.width();
        let pad = body_width.saturating_sub(left_w + meta_w);
        let spans = vec![
            Span::styled(left, Style::default().bg(bg).fg(fg)),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            Span::styled(meta, Style::default().bg(bg).fg(muted)),
        ];
        body.push(Line::from(spans));
    }

    render_body(frame, f.body, body, &mut 0, Some(selected), false, theme);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓ navigate · Enter open · d delete · Esc close",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}

/// Draw the session-context modal: a tabbed overview of the live session.
///
/// Layout: a borderless panel (no `┃` bar) with a 2-column left/right and
/// 1-row top/bottom inner padding, split into three sections —
/// **header** (`Session` title + tab labels on one row, tabs tinted to the
/// brand color so the active pane reads as part of the session), **body**
/// (the active pane's content, scrollable via `scroll` so long lists never get
/// silently truncated), and **footer** (keybinding hints).
///
/// `session_context` is the latest snapshot from the harness; when `None`
/// (still loading) every pane except Model/MCP shows a placeholder. Model +
/// MCP additionally fall back to the App-level `provider`/`model`/`key_status`/
/// `mcp_statuses` so they render immediately on open. `modal_index` is the row
/// cursor for the list panes (Skills / Permissions / Tools); the body
/// auto-scrolls to keep it visible. `scroll` is read AND written back (clamped
/// to the body's real height), so the caller's stored offset stays consistent.
#[allow(clippy::too_many_arguments)]
pub fn draw_session_modal(
    frame: &mut Frame,
    tab: crate::tui::SessionTab,
    provider: &str,
    model: &str,
    key_status: &HashMap<String, bool>,
    mcp_statuses: &[(String, neenee_core::mcp::McpConnectionStatus)],
    session_context: Option<&neenee_core::SessionContextSnapshot>,
    modal_index: usize,
    scroll: &mut usize,
    theme: &Theme,
) {
    let modal = centered_rect(76, 70, viewport_rect(frame));
    let f = modal_frame(frame, modal, theme.panel(), true, true);
    let header_rect = f.header;
    let body_rect = f.body;

    // ── Header: "Session" + tabs on one row ──
    let mut header_spans: Vec<Span> = vec![
        Span::styled(
            "Session",
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
    ];
    for (i, variant) in crate::tui::SessionTab::ALL.iter().enumerate() {
        if i > 0 {
            header_spans.push(Span::raw("  "));
        }
        let active = *variant == tab;
        let mut style = Style::default();
        if active {
            style = style
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        } else {
            style = style.fg(theme.muted());
        }
        header_spans.push(Span::styled(variant.label(), style));
    }
    if let Some(h) = header_rect {
        frame.render_widget(Paragraph::new(Line::from(header_spans)), h);
    }

    // ── Body: collect the active pane's lines, then scroll+clip ──
    let label_w = 12usize;
    let label = |k: &str| {
        Span::styled(
            format!("{:<w$}", k, w = label_w),
            Style::default().fg(theme.muted()),
        )
    };
    let window_label = |window: usize| -> String {
        if window >= 1_000_000 {
            format!("{:.1}M tokens", window as f64 / 1_000_000.0)
        } else if window >= 1_000 {
            format!("{:.0}k tokens", window as f64 / 1_000.0)
        } else {
            "unknown".to_string()
        }
    };

    let mut body: Vec<Line> = Vec::new();
    let is_list_pane = matches!(
        tab,
        crate::tui::SessionTab::Skills
            | crate::tui::SessionTab::Permissions
            | crate::tui::SessionTab::Tools
    );

    match tab {
        crate::tui::SessionTab::Model => {
            let (p_name, m_display, m_id, window, key_ok, desc, caps) = match session_context {
                Some(s) => (
                    s.model.provider.clone(),
                    s.model.display_name.clone(),
                    s.model.model.clone(),
                    s.model.context_window,
                    s.model.api_key_ready,
                    s.model.description.clone(),
                    s.model.capabilities.clone(),
                ),
                None => {
                    let solution = crate::tui::PROVIDERS.iter().find(|x| x.id == provider);
                    (
                        provider.to_string(),
                        crate::tui::model_display_name(model),
                        model.to_string(),
                        crate::tui::provider_context_window(provider),
                        key_status
                            .get(&provider.to_lowercase())
                            .copied()
                            .unwrap_or(false),
                        solution
                            .map(|s| s.description.to_string())
                            .unwrap_or_default(),
                        Vec::new(),
                    )
                }
            };
            let (key_mark, key_fg, key_word) = if key_ok {
                ("✓", theme.ok(), "ready")
            } else {
                ("✗", theme.err(), "no key")
            };
            body.push(Line::from(vec![
                label("Provider"),
                Span::styled(p_name, Style::default().fg(theme.fg())),
            ]));
            body.push(Line::from(vec![
                label("Model"),
                Span::styled(
                    m_display,
                    Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  ({})", m_id), Style::default().fg(theme.muted())),
            ]));
            body.push(Line::from(vec![
                label("Context"),
                Span::styled(window_label(window), Style::default().fg(theme.fg())),
            ]));
            body.push(Line::from(vec![
                label("API key"),
                Span::styled(
                    format!("{} ", key_mark),
                    Style::default().fg(key_fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(key_word, Style::default().fg(key_fg)),
            ]));
            if !caps.is_empty() {
                body.push(Line::from(vec![
                    label("Capabilities"),
                    Span::styled(caps.join(" · "), Style::default().fg(theme.fg())),
                ]));
            }
            if !desc.is_empty() {
                body.push(Line::from(""));
                body.push(Line::from(Span::styled(
                    desc,
                    Style::default().fg(theme.muted()),
                )));
            }
        }
        crate::tui::SessionTab::Mcp => {
            let servers: Vec<(String, McpRow)> = match session_context {
                Some(s) => s
                    .mcp
                    .iter()
                    .map(|srv| {
                        let row = if srv.disabled {
                            McpRow::disabled()
                        } else if let Some(reason) = srv.failure.as_ref() {
                            McpRow::failed(reason.clone())
                        } else if srv.connected {
                            McpRow::connected(srv.tool_names.clone())
                        } else {
                            McpRow::disabled()
                        };
                        (srv.name.clone(), row)
                    })
                    .collect(),
                None => mcp_statuses
                    .iter()
                    .map(|(name, status)| {
                        use neenee_core::mcp::McpConnectionStatus;
                        let row = match status {
                            McpConnectionStatus::Connected { tools } => {
                                McpRow::connected(vec![format!("+{} more", tools)])
                            }
                            McpConnectionStatus::Disabled => McpRow::disabled(),
                            McpConnectionStatus::Failed(r) => McpRow::failed(r.clone()),
                        };
                        (name.clone(), row)
                    })
                    .collect(),
            };
            if servers.is_empty() {
                body.push(Line::from(Span::styled(
                    "No MCP servers configured.",
                    Style::default().fg(theme.muted()),
                )));
                body.push(Line::from(""));
                body.push(Line::from(Span::styled(
                    "Add [mcp.<name>] tables to ~/.config/neenee/config.toml.",
                    Style::default().fg(theme.muted()),
                )));
            } else {
                for (name, row) in &servers {
                    let (word, color) = row.summary(theme);
                    body.push(Line::from(vec![
                        Span::styled(format!("{:<18}", name), Style::default().fg(theme.fg())),
                        Span::styled(word, Style::default().fg(color)),
                    ]));
                    if let Some(detail) = row.detail() {
                        body.push(Line::from(Span::styled(
                            format!("    {}", detail),
                            Style::default().fg(theme.muted()),
                        )));
                    }
                }
            }
        }
        crate::tui::SessionTab::Skills => {
            let skills = session_context.map(|s| s.skills.as_slice()).unwrap_or(&[]);
            if skills.is_empty() {
                body.push(placeholder(
                    "Loading skills…",
                    session_context.is_some(),
                    theme.muted(),
                ));
            } else {
                for (i, skill) in skills.iter().enumerate() {
                    body.push(selectable_row(
                        i,
                        modal_index,
                        &skill.name,
                        &skill.description,
                        skill.enabled,
                        "enabled",
                        "disabled",
                        theme,
                    ));
                }
            }
        }
        crate::tui::SessionTab::Permissions => {
            let rules = session_context
                .map(|s| s.permissions.as_slice())
                .unwrap_or(&[]);
            if rules.is_empty() {
                body.push(placeholder(
                    "No always-allow rules cached this session.",
                    session_context.is_some(),
                    theme.muted(),
                ));
            } else {
                for (i, rule) in rules.iter().enumerate() {
                    let summary = format!("{} {}", rule.tool, rule.scope);
                    body.push(selectable_row(
                        i,
                        modal_index,
                        &summary,
                        "Space revokes this rule",
                        true,
                        "allowed",
                        "",
                        theme,
                    ));
                }
            }
        }
        crate::tui::SessionTab::Tools => {
            let tools = session_context.map(|s| s.tools.as_slice()).unwrap_or(&[]);
            if tools.is_empty() {
                body.push(placeholder(
                    "Loading tools…",
                    session_context.is_some(),
                    theme.muted(),
                ));
            } else {
                for (i, tool) in tools.iter().enumerate() {
                    body.push(selectable_row(
                        i,
                        modal_index,
                        &tool.name,
                        &tool.source,
                        tool.enabled,
                        "on",
                        "off",
                        theme,
                    ));
                }
            }
        }
    }

    // Scroll + clip via the shared helper. `follow` keeps the selected row in
    // view on list panes; read-only panes scroll independently via `*scroll`.
    let visible = body_rect.height as usize;
    let content_lines = body.len();
    let follow = if is_list_pane {
        Some(modal_index)
    } else {
        None
    };
    render_body(frame, body_rect, body, scroll, follow, false, theme);

    // ── Footer ──
    let interactive = matches!(
        tab,
        crate::tui::SessionTab::Permissions | crate::tui::SessionTab::Tools
    );
    let scrollable = content_lines > visible;
    let mut hint = String::from("← → switch tab");
    if interactive {
        hint.push_str(" · ↑↓ select · Space act");
    } else if scrollable {
        hint.push_str(" · ↑↓ scroll");
    }
    hint.push_str(" · Esc close");
    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}
