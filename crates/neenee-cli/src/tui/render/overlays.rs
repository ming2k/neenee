//! Overlay modal renderers: provider picker, sessions, history search, the
//! permission sheet, API-key / endpoint / model-name prompts, the help overlay,
//! and the copy / armed toasts. Plus the relative-time formatter used by the
//! sessions list.

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::layout::LayoutMap;
use neenee_core::{PermissionRequest, ProviderPickerSnapshot, UserQuestionRequest};

use super::primitives::{
    centered_rect, contrast_fg, modal_frame, panel_block, render_body, viewport_rect,
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

/// Draw the provider picker The input line is borrowed as a
/// fuzzy filter; rows are sorted favorites-first → last-used → name. `Enter`
/// activates (the default on an empty filter, the highlighted row otherwise);
/// `*` toggles a favorite.
///
/// `query` / `cursor_position` are the borrowed filter (the composer input
/// while the modal is open). `key_status` is the legacy per-provider key map,
/// retained for the readiness glyph. `provider_picker` carries the favorite /
/// last-used / default signals.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    solutions: &[crate::tui::ProviderPreset],
    current_provider: &str,
    modal_index: usize,
    key_status: &HashMap<String, bool>,
    provider_picker: &ProviderPickerSnapshot,
    query: &str,
    cursor_position: usize,
    theme: &Theme,
) {
    let area = centered_rect(72, 60, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    // Filter + sort once for the frame; the input handler shares the same
    // function so selection and rendering never diverge.
    let ranked = crate::tui::providers_filtered_from(solutions, provider_picker, query.trim());

    let header_rect = f.header;
    if let Some(h) = header_rect {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Models",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("❯ ", Style::default().fg(theme.muted())),
                Span::styled(
                    if query.is_empty() {
                        "type to filter · enter selects default"
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
            ])),
            h,
        );
    }

    let mut body: Vec<Line> = Vec::new();
    if ranked.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )));
    } else {
        for (row, (sol_idx, picker_row)) in ranked.iter().enumerate() {
            let solution = &solutions[*sol_idx];
            let is_current = solution.id == current_provider;
            let is_selected = row == modal_index;
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
            let dim = if is_selected {
                Style::default().bg(row_bg).fg(contrast_fg(theme.brand()))
            } else {
                Style::default().fg(theme.muted())
            };
            let star = if picker_row.favorite { "★ " } else { "  " };
            let dot = if is_current { "● " } else { "  " };
            let (key_label, key_color) = match key_status.get(solution.id) {
                Some(true) => ("✓", theme.ok()),
                Some(false) => ("✗", theme.err()),
                None => ("", row_fg),
            };
            let key_style = if is_selected {
                Style::default().bg(row_bg).fg(contrast_fg(theme.brand()))
            } else {
                Style::default().fg(key_color)
            };
            let star_style = if picker_row.favorite {
                Style::default().bg(row_bg).fg(if is_selected {
                    contrast_fg(theme.brand())
                } else {
                    theme.warn()
                })
            } else {
                dim
            };
            body.push(Line::from(vec![
                Span::styled(format!(" {}", star), star_style),
                Span::styled(dot.to_string(), dim),
                Span::styled(
                    format!("{:<16} ", solution.name),
                    base.add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<2} ", key_label), key_style),
                Span::styled(format!("{} ", solution.model), dim),
                Span::styled(format!("· {}", solution.description), dim),
            ]));
        }
    }
    render_body(frame, f.body, body, &mut 0, Some(modal_index));

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "type to filter · ↑↓ navigate · enter activate · * favorite · esc",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }

    // Place the real terminal caret in the filter field (the header row, after
    // the `Models  ❯ ` prefix) so typing visibly tracks the insertion point.
    if let Some(h) = header_rect {
        let prefix = "Models  ❯ ".width() as u16;
        let cursor_x = h.x + prefix + caret_column(query, cursor_position);
        let cursor_y = h.y;
        frame.set_cursor(cursor_x, cursor_y);
    }
}

/// Display column of the caret within a rendered input field, given its char
/// index. Each modal field renders its own masked/verbatim `display` string,
/// so mapping through chars (not bytes) keeps wide glyphs and `•` masks right.
fn caret_column(display: &str, cursor_position: usize) -> u16 {
    let n = cursor_position.min(display.chars().count());
    let byte = display
        .char_indices()
        .nth(n)
        .map(|(i, _)| i)
        .unwrap_or(display.len());
    display[..byte].width() as u16
}

/// Draw the unified provider editor Two fields — API key
/// (masked) and model id — with `Tab` cycling focus. The composer input line
/// is borrowed for the focused field's value; `key_buf` / `model_buf` hold the
/// other field while it is unfocused. `field` is `0` for key, `1` for model id.
#[allow(clippy::too_many_arguments)]
pub fn draw_model_editor(
    frame: &mut Frame,
    title: &str,
    field: u8,
    key_buf: &str,
    model_buf: &str,
    input: &str,
    cursor_position: usize,
    theme: &Theme,
) {
    let area = centered_rect(60, 36, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    // The focused field's value lives in `input`; the unfocused one in its buf.
    let (key_display, model_display): (String, String) = if field == 0 {
        (input.to_string(), model_buf.to_string())
    } else {
        // Mask the key whenever it is not being actively edited.
        ("•".repeat(key_buf.chars().count()), input.to_string())
    };

    let field_label = |label: &str, focused: bool| {
        if focused {
            Span::styled(
                format!(" {:<8}", label),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {:<8}", label), Style::default().fg(theme.muted()))
        }
    };
    let value_style = |focused: bool| {
        if focused {
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        }
    };

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("Edit · {}", title),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let body = vec![
        Line::from(vec![
            field_label("API key", field == 0),
            Span::styled(
                if key_display.is_empty() && field == 0 {
                    "enter key…".to_string()
                } else {
                    key_display.clone()
                },
                value_style(field == 0),
            ),
        ]),
        Line::from(vec![
            field_label("Model id", field == 1),
            Span::styled(
                if model_display.is_empty() && field == 1 {
                    "enter model id…".to_string()
                } else {
                    model_display.clone()
                },
                value_style(field == 1),
            ),
        ]),
    ];
    let body_rect = f.body;
    render_body(frame, body_rect, body, &mut 0, None);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "tab switch field · enter save & switch · esc cancel",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }

    // Place the terminal caret on the focused field's value row, after its
    // label. API key is body line 0, Model id is body line 1 (the gap below
    // the header is provided by `modal_frame`, so the body starts at the top
    // of the body rect).
    let prefix = format!(" {:<8}", if field == 0 { "API key" } else { "Model id" });
    let cursor_x = body_rect.x + prefix.width() as u16 + caret_column(input, cursor_position);
    let cursor_y = body_rect.y + if field == 0 { 0 } else { 1 };
    frame.set_cursor(cursor_x, cursor_y);
}

/// Compact relative time for space-constrained surfaces (e.g. the sessions
/// picker's meta column): `now` / `3m` / `2h` / `5d` / `3w` — no "ago" suffix.
pub fn relative_time_compact(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(ts);
    if diff < 60 {
        "now".to_string()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else if diff < 7 * 86_400 {
        format!("{}d", diff / 86_400)
    } else {
        format!("{}w", diff / (7 * 86_400))
    }
}

/// Truncate `s` to fit `max` display columns, appending `…` when it doesn't.
/// Width-aware so CJK/wide glyphs don't break the column budget. Used by table-
/// like modal rows to cap a long first column and leave room for the rest.
pub fn truncate_ellipsis(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0).max(1);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

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

    render_body(frame, f.body, body, &mut 0, Some(selected));

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
    render_body(frame, body_rect, body, scroll, follow);

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

/// One MCP server row, unpacked for rendering. `Connected` carries the
/// per-server tool names so the MCP pane can list them rather than just a count.
enum McpRow {
    Connected(Vec<String>),
    Disabled,
    Failed(String),
}

impl McpRow {
    fn connected(tools: Vec<String>) -> Self {
        Self::Connected(tools)
    }
    fn disabled() -> Self {
        Self::Disabled
    }
    fn failed(reason: String) -> Self {
        Self::Failed(reason)
    }

    /// One-line status summary + color, shown next to the server name.
    fn summary(&self, theme: &Theme) -> (String, Color) {
        match self {
            Self::Connected(tools) => (format!("Connected · {} tools", tools.len()), theme.ok()),
            Self::Disabled => ("Disabled".to_string(), theme.muted()),
            Self::Failed(reason) => (format!("Failed: {}", reason), theme.err()),
        }
    }

    /// Optional second line (the tool-name list for a connected server).
    fn detail(&self) -> Option<String> {
        match self {
            Self::Connected(tools) if !tools.is_empty() => {
                let names: String = tools.join(", ");
                Some(format!("tools: {}", names))
            }
            _ => None,
        }
    }
}

/// Build a selectable list row: `▣ name  hint` with the selected row taking
/// the brand background. `state_on`/`state_off` label the enabled state shown
/// at the row's right edge; an empty `state_off` hides the badge entirely.
#[allow(clippy::too_many_arguments)]
fn selectable_row(
    i: usize,
    selected: usize,
    name: &str,
    hint: &str,
    enabled: bool,
    state_on: &str,
    state_off: &str,
    theme: &Theme,
) -> Line<'static> {
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
    let mark = if enabled { "●" } else { "○" };
    let state = if enabled { state_on } else { state_off };
    let mut spans = vec![
        Span::styled(format!("{} ", mark), Style::default().bg(bg).fg(fg)),
        Span::styled(name.to_string(), Style::default().bg(bg).fg(fg)),
    ];
    if !hint.is_empty() {
        spans.push(Span::styled(
            format!("  {}", hint),
            Style::default().bg(bg).fg(muted),
        ));
    }
    if !state.is_empty() {
        spans.push(Span::styled(
            format!("  [{}]", state),
            Style::default().bg(bg).fg(muted),
        ));
    }
    Line::from(spans)
}

/// Empty-list placeholder: a muted message, tuned to whether the snapshot has
/// arrived (`loaded` = true → genuinely empty; false → still loading).
fn placeholder(message: &str, loaded: bool, muted: Color) -> Line<'static> {
    let text = if loaded {
        message.to_string()
    } else {
        "Loading…".to_string()
    };
    Line::from(Span::styled(text, Style::default().fg(muted)))
}

/// Draw the history search modal.
///
/// `query` is the fuzzy query the user is typing into the (borrowed) input
/// box; `ranked` is the pre-computed `(original_history_index, FuzzyMatch)`
/// list produced by [`crate::tui::App::history_filtered`] — passing it in avoids a
/// second fuzzy pass per frame. `modal_index` selects into `ranked`.
///
/// Each result line highlights the matched characters of the query so the
/// user can see why an entry surfaced. Empty query → show everything with no
/// highlights; query with no matches → "no matches" placeholder.
#[allow(clippy::too_many_arguments)]
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    query: &str,
    cursor_position: usize,
    ranked: &[(usize, crate::tui::fuzzy::FuzzyMatch)],
    modal_index: usize,
    theme: &Theme,
) {
    let area = centered_rect(70, 55, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let header_rect = f.header;
    if let Some(h) = header_rect {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Input History",
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
            ])),
            h,
        );
    }

    let mut body: Vec<Line> = Vec::new();
    if history.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no history yet — send a message to populate this list)",
            Style::default().fg(theme.muted()),
        )));
    } else if ranked.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )));
    } else {
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
            body.push(Line::from(spans));
        }
    }
    render_body(frame, f.body, body, &mut 0, Some(modal_index));

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "type to filter · ↑↓ navigate · Enter insert · Esc close",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }

    // Place the real terminal caret in the filter field (the header row, after
    // the `Input History  ❯ ` prefix). The composer underneath is skipped for
    // this modal, so without this the caret would be absent.
    if let Some(h) = header_rect {
        let prefix = "Input History  ❯ ".width() as u16;
        let cursor_x = h.x + prefix + caret_column(query, cursor_position);
        let cursor_y = h.y;
        frame.set_cursor(cursor_x, cursor_y);
    }
}

/// Draw a centered user-question modal. Shows one question at a time with its
/// options; the user navigates with ↑/↓, selects with Space or Enter, and
/// submits with Enter. Multi-select questions use checkboxes; single-select
/// uses radio buttons. A numbered digit key (1-9) jumps directly to an option.
const OTHER_OPTION_LABEL: &str = "Other";
const OTHER_OPTION_PLACEHOLDER: &str = "Type your own answer";

pub fn draw_question_modal(
    frame: &mut Frame,
    request: &UserQuestionRequest,
    current_question: usize,
    selected: &[Vec<usize>],
    other_text: &[String],
    highlighted: usize,
    theme: &Theme,
) {
    let area = centered_rect(78, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let question = request.questions.get(current_question);
    let total = request.questions.len();

    if let Some(h) = f.header {
        let title = if total > 1 {
            format!("Question {}/{}", current_question + 1, total)
        } else {
            "Question".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title,
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let mut body: Vec<Line> = Vec::new();
    if let Some(q) = question {
        if let Some(header) = &q.header {
            body.push(Line::from(vec![Span::styled(
                format!(" {}", header),
                Style::default()
                    .fg(theme.info())
                    .add_modifier(Modifier::BOLD),
            )]));
        }
        body.push(Line::from(vec![Span::styled(
            format!(" {}", q.question),
            Style::default().fg(theme.fg()),
        )]));
        body.push(Line::from(""));

        let q_selected = selected.get(current_question);
        let other_index = q.options.len();
        let other_highlighted = highlighted == other_index;
        let other_text_value = other_text
            .get(current_question)
            .map(String::as_str)
            .unwrap_or("");

        for (i, option) in q.options.iter().enumerate() {
            let is_selected = q_selected.is_some_and(|s| s.contains(&i));
            let is_highlighted = i == highlighted;
            render_question_option(
                &mut body,
                i,
                &option.label,
                option.description.as_deref(),
                is_selected,
                is_highlighted,
                q.multi_select,
                theme,
            );
        }

        render_question_option(
            &mut body,
            other_index,
            OTHER_OPTION_LABEL,
            None,
            q_selected.is_some_and(|s| s.contains(&other_index)),
            other_highlighted,
            q.multi_select,
            theme,
        );
        if other_highlighted {
            let display = if other_text_value.is_empty() {
                OTHER_OPTION_PLACEHOLDER
            } else {
                other_text_value
            };
            body.push(Line::from(vec![
                Span::styled("   ", Style::default().fg(theme.fg())),
                Span::styled(
                    format!("{} {}", "▏", display),
                    Style::default()
                        .fg(if other_text_value.is_empty() {
                            theme.muted()
                        } else {
                            theme.fg()
                        })
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), f.body);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓ navigate · Space toggle · 1-9 jump · Enter submit · Esc cancel",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_question_option(
    lines: &mut Vec<Line>,
    index: usize,
    label: &str,
    description: Option<&str>,
    is_selected: bool,
    is_highlighted: bool,
    multi_select: bool,
    theme: &Theme,
) {
    let marker = if multi_select {
        if is_selected {
            "[x]"
        } else {
            "[ ]"
        }
    } else {
        if is_selected {
            "●"
        } else {
            "○"
        }
    };
    let number = if index < 9 {
        format!("{}.", index + 1)
    } else {
        " ".to_string()
    };
    let focus_style = if is_highlighted {
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted())
    };
    let marker_style = if is_selected {
        Style::default().fg(theme.ok()).add_modifier(Modifier::BOLD)
    } else {
        focus_style
    };
    let text_style = if is_highlighted {
        Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg())
    };
    let focus = if is_highlighted { "❯" } else { " " };

    let label_line = Line::from(vec![
        Span::styled(format!("{} {:>2} ", focus, number), focus_style),
        Span::styled(format!("{} ", marker), marker_style),
        Span::styled(label.to_string(), text_style),
    ]);
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(label_line);

    if let Some(desc) = description {
        let desc_style = Style::default().fg(theme.dim());
        let indent = if multi_select { "         " } else { "       " };
        lines.push(Line::from(vec![
            Span::styled(indent.to_string(), desc_style),
            Span::styled(desc.to_string(), desc_style),
        ]));
    }
}

/// Draw a blocking tool permission request inline, replacing the composer
/// (input box) area. The transcript above stays visible and scrollable.
///
/// Collapsed (the default) the sheet is a one-line summary — the tool name
/// plus its scope (the specific path/command being touched) — followed by a
/// footer of inline actions. Selecting "Details" expands the body upward to
/// reveal the full description and arguments without leaving the prompt.
#[allow(clippy::too_many_arguments)]
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

    // Header line: human-friendly label (falling back to the raw tool name
    // for safety), plus the concrete scope (path/command) when it adds
    // information. The scope is the single most useful detail, so it earns a
    // spot in the collapsed summary; everything else hides behind "Details".
    // The left bar carries the severity cue.
    let label = if request.label.is_empty() {
        request.tool.clone()
    } else {
        request.label.clone()
    };
    let mut header = vec![Span::styled(
        label,
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
        body_lines.extend(
            arguments
                .lines()
                .map(|line| Line::from(line).style(Style::default().fg(theme.code_text()))),
        );
    }

    let fixed = PERMISSION_TOP_PADDING + PERMISSION_BODY_FOOTER_GAP + PERMISSION_FOOTER_HEIGHT;
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
    let sheet_h = desired_h.max(input_rect.height).min(area_bottom).max(1);
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
    msg: &crate::tui::document::TranscriptMessage,
    scroll: u16,
    theme: &Theme,
) {
    use crate::tui::document::MessageKind;
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
    let marker_style = Style::default()
        .fg(theme.warn())
        .add_modifier(Modifier::BOLD);
    match &msg.kind {
        MessageKind::ToolStep { structured, .. }
            if matches!(
                structured.as_deref(),
                Some(neenee_core::ToolOutput::Shell { .. })
            ) =>
        {
            let MessageKind::ToolStep { structured, .. } = &msg.kind else {
                unreachable!()
            };
            let neenee_core::ToolOutput::Shell {
                command,
                stdout,
                stderr,
                exit,
                truncated,
            } = structured.as_deref().expect("guarded by match guard")
            else {
                unreachable!()
            };
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
            if let Some(code) = exit.filter(|c| *c != 0) {
                lines.push(Line::from(Span::styled(
                    format!("exit {}", code),
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
    let area = centered_rect(58, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Help",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

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

    let body = vec![
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("alt+enter", "insert newline (ctrl+j)"),
        row("esc", "interrupt (×2) / close"),
        row("ctrl+c", "copy · interrupt · quit (×2)"),
        Line::from(""),
        Line::from(section("Line editing")),
        row("ctrl+a / ctrl+e", "caret to line start / end"),
        row("home / end", "caret to line start / end"),
        row("ctrl+u / ctrl+k", "delete to line start / end"),
        row("ctrl+w", "delete previous word"),
        row("alt+backspace", "delete previous word"),
        row("alt+d", "delete next word"),
        row("ctrl+← / ctrl+→", "move word back / forward"),
        row("alt+b / alt+f", "move word back / forward"),
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
        row("/session", "session context"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("/", "slash commands"),
        Line::from(""),
        Line::from(section("Modes")),
        row("/mode", "build · plan"),
        row("/pursue", "pursue a pursuit until it is met"),
        Line::from(""),
        Line::from(desc("Drag to select · Ctrl+C or Ctrl+Shift+C to copy.")),
    ];
    render_body(frame, f.body, body, &mut 0, None);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "esc · close",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}

/// Draw a "copied to clipboard" toast.
pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.size();
    let color = if failed { theme.err() } else { theme.ok() };
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

/// Read-only preview modal for the active plan file. Reached by clicking
/// the sticky plan panel above the input box or pressing `Ctrl+P`. The
/// caller caches the file content in `App::plan_preview_content` at open
/// time so the modal does not hit disk on every redraw; `scroll` is the
/// overlay's own scroll offset, reset on each open.
pub fn draw_plan_preview_modal(frame: &mut Frame, content: &str, scroll: u16, theme: &Theme) {
    // Wider than the help modal — plans are usually long. Cap at 80% x 70%
    // so the modal stays readable on small terminals.
    let area = centered_rect(80, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Plan preview",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    // Render the cached plan content verbatim (no markdown styling) so the
    // user sees exactly what the model wrote. Lines that exceed the body
    // width are wrapped by ratatui's `Paragraph` at word boundaries; the
    // `scroll` parameter shifts the view down.
    let lines: Vec<Line> = content
        .lines()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(theme.fg()))))
        .collect();
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), f.body);

    if let Some(footer) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Esc to close · ↑/↓ scroll · Ctrl+P toggles",
                Style::default().fg(theme.muted()),
            ))),
            footer,
        );
    }
}

/// Inputs for [`draw_activity_modal`]. Carries everything the old always-pinned
/// pursuit bar, plan panel, and activity bar used to show, gathered into one
/// overlay so the footer is a single line. Fields are `Option`al so the modal
/// simply omits a section when there is nothing to report.
pub struct ActivityModalView<'a> {
    /// Active pursuit, if any. Shown as an objective line plus one row per
    /// checklist item.
    pub pursuit: Option<&'a neenee_core::Pursuit>,
    /// Live unified task list, if any. Shown as a header (done/total) plus
    /// one row per item with a status glyph.
    pub todos: Option<&'a neenee_core::TodoList>,
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
    /// Live activity status string (e.g. `searching codebase`), or empty/idle.
    pub activity: &'a str,
}

/// Foreground color for a todo-status glyph. Completed/in-progress pop in
/// `ok`/`warn`; pending/cancelled stay muted so the eye is drawn to active
/// work.
fn todo_status_glyph_color(status: neenee_core::TodoStatus, theme: &Theme, muted: Color) -> Color {
    use neenee_core::TodoStatus;
    match status {
        TodoStatus::Completed => theme.ok(),
        TodoStatus::InProgress => theme.warn(),
        TodoStatus::Pending | TodoStatus::Cancelled => muted,
    }
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
        pursuit,
        todos,
        turn_count,
        current_round,
        review_alert,
        current_model,
        turn_started_at,
        activity,
    } = view;

    let area = centered_rect(72, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Activity",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let muted = theme.muted();
    let mut lines: Vec<Line<'static>> = Vec::new();
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

    // ── Tasks ──
    if let Some(list) = todos.filter(|l| !l.items.is_empty()) {
        if have_section {
            lines.push(Line::from(""));
        }
        have_section = true;
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
    }

    // ── Activity (always shown) ──
    if have_section {
        lines.push(Line::from(""));
    }
    let idle = activity.is_empty() || activity == "idle";
    lines.push(Line::from(vec![Span::styled(
        "Activity",
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )]));

    // Structural detail line: `turn N · round M · <model> · <elapsed>`.
    // Omitted entirely before the first turn so the section reads as "idle".
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
                super::chrome::format_elapsed(started.elapsed()),
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

    render_body(frame, f.body, lines, scroll, None);

    if let Some(footer) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Esc to close · ↑/↓ scroll",
                Style::default().fg(theme.muted()),
            ))),
            footer,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ellipsis_keeps_short_strings_untouched() {
        assert_eq!(truncate_ellipsis("hi", 10), "hi");
        assert_eq!(truncate_ellipsis("hi", 2), "hi");
        assert_eq!(truncate_ellipsis("", 5), "");
    }

    #[test]
    fn truncate_ellipsis_appends_ellipsis_on_overflow() {
        // "hello world" (11 cols) capped at 6 -> 5 chars + …
        assert_eq!(truncate_ellipsis("hello world", 6), "hello…");
    }

    #[test]
    fn truncate_ellipsis_degenerate_widths() {
        assert_eq!(truncate_ellipsis("abc", 0), "");
        assert_eq!(truncate_ellipsis("abc", 1), "…");
    }

    #[test]
    fn truncate_ellipsis_is_width_aware_for_cjk() {
        // 你好 = 4 display cols. Capped at 3 -> only 你 (2 cols) fits before the ….
        assert_eq!(truncate_ellipsis("你好", 3), "你…");
    }

    #[test]
    fn relative_time_compact_drops_ago_suffix() {
        // now
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        assert_eq!(relative_time_compact(t), "now");
        // 2 hours ago -> "2h" (no "ago")
        let t = t.saturating_sub(2 * 3600);
        assert_eq!(relative_time_compact(t), "2h");
    }
}
