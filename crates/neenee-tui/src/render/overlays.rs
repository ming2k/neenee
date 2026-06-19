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
use neenee_core::{ModelPickerSnapshot, PermissionRequest, UserQuestionRequest};

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

/// Draw the models picker (ADR-0002 phase 3). The input line is borrowed as a
/// fuzzy filter; rows are sorted favorites-first → last-used → name. `Enter`
/// activates (the default on an empty filter, the highlighted row otherwise);
/// `*` toggles a favorite.
///
/// `query` / `cursor_position` are the borrowed filter (the composer input
/// while the modal is open). `key_status` is the legacy per-provider key map,
/// retained for the readiness glyph. `model_picker` carries the favorite /
/// last-used / default signals.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    solutions: &[crate::ModelSolution],
    current_provider: &str,
    modal_index: usize,
    key_status: &HashMap<String, bool>,
    model_picker: &ModelPickerSnapshot,
    query: &str,
    cursor_position: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(72, 60, viewport_rect(frame));
    frame.render_widget(Clear, area);

    // Filter + sort once for the frame; the input handler shares the same
    // function so selection and rendering never diverge.
    let ranked = crate::models_filtered_from(solutions, model_picker, query.trim());

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(
            " Models",
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
    ])];

    if ranked.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  (no matches — try a shorter or different query)",
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
            // Leading markers: favorite star, then default (current) dot.
            let star = if picker_row.favorite { "★ " } else { "  " };
            let dot = if is_current { "● " } else { "  " };
            // Key readiness glyph.
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
            // Favorite star gets the accent color off the selection row.
            let star_style = if picker_row.favorite {
                Style::default().bg(row_bg).fg(if is_selected {
                    contrast_fg(theme.brand())
                } else {
                    theme.warn()
                })
            } else {
                dim
            };
            lines.push(Line::from(vec![
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
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " type to filter · ↑↓ navigate · enter activate · * favorite · esc ",
        Style::default().fg(theme.muted()),
    )));

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);

    // Place the real terminal caret in the filter field (line 0 of the body,
    // after the ` Models  ❯ ` prefix) so typing visibly tracks the insertion
    // point, matching the history-search modal.
    let prefix = " Models  ❯ ".width() as u16;
    let cursor_x = area.x + 1 + prefix + caret_column(query, cursor_position);
    let cursor_y = area.y;
    frame.set_cursor(cursor_x, cursor_y);
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

/// Draw the unified model editor (ADR-0002 phase 4). Two fields — API key
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
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(60, 36, viewport_rect(frame));
    frame.render_widget(Clear, area);

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
            Span::styled(
                format!(" {:<8}", label),
                Style::default().fg(theme.muted()),
            )
        }
    };
    let value_style = |focused: bool| {
        if focused {
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        }
    };

    let lines = vec![
        Line::from(Span::styled(
            format!(" Edit · {}", title),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
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
        Line::from(""),
        Line::from(Span::styled(
            " tab switch field · enter save & switch · esc cancel ",
            Style::default().fg(theme.muted()),
        )),
    ];

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);

    // Place the terminal caret on the focused field's value row, after its
    // label. Key is row 2, model id is row 3 of the body.
    let prefix = format!(" {:<8}", if field == 0 { "API key" } else { "Model id" });
    let cursor_x = area.x + 1 + prefix.width() as u16 + caret_column(input, cursor_position);
    let cursor_y = if field == 0 { area.y + 2 } else { area.y + 3 };
    frame.set_cursor(cursor_x, cursor_y);
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
    cursor_position: usize,
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

    // Place the real terminal caret in the filter field (line 0 of the body,
    // after the ` Input History  ❯ ` prefix) so typing visibly tracks the
    // insertion point. The composer underneath is skipped for this modal, so
    // without this the caret would be absent. panel_block only draws a 1-cell
    // left border, so the body starts one column in.
    let prefix = " Input History  ❯ ".width() as u16;
    let cursor_x = area.x + 1 + prefix + caret_column(query, cursor_position);
    let cursor_y = area.y;
    frame.set_cursor(cursor_x, cursor_y);
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
    draw_dim_backdrop(frame, frame.size(), theme.backdrop());
    let area = centered_rect(78, 70, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let question = request.questions.get(current_question);
    let total = request.questions.len();
    let mut lines: Vec<Line> = Vec::new();

    // Title with progress
    let title = if total > 1 {
        format!(" Question {}/{}", current_question + 1, total)
    } else {
        " Question".to_string()
    };
    lines.push(Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));

    if let Some(q) = question {
        if let Some(header) = &q.header {
            lines.push(Line::from(vec![Span::styled(
                format!(" {}", header),
                Style::default()
                    .fg(theme.info())
                    .add_modifier(Modifier::BOLD),
            )]));
        }
        lines.push(Line::from(vec![Span::styled(
            format!(" {}", q.question),
            Style::default().fg(theme.fg()),
        )]));
        lines.push(Line::from(""));

        let q_selected = selected.get(current_question);
        let other_index = q.options.len();
        let other_highlighted = highlighted == other_index;
        let other_text_value = other_text
            .get(current_question)
            .map(String::as_str)
            .unwrap_or("");

        for (i, option) in q.options.iter().enumerate() {
            let is_selected = q_selected.map_or(false, |s| s.contains(&i));
            let is_highlighted = i == highlighted;
            render_question_option(
                &mut lines,
                i,
                &option.label,
                option.description.as_deref(),
                is_selected,
                is_highlighted,
                q.multi_select,
                theme,
            );
        }

        // Automatic "Other" option.
        render_question_option(
            &mut lines,
            other_index,
            OTHER_OPTION_LABEL,
            Some(OTHER_OPTION_PLACEHOLDER),
            q_selected.map_or(false, |s| s.contains(&other_index)),
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
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default().fg(theme.fg())),
                Span::styled(
                    format!(
                        "{} {}",
                        if other_text_value.is_empty() {
                            "▏"
                        } else {
                            "▏"
                        },
                        display
                    ),
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

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        " ↑↓ navigate · Space/Enter toggle · 1-9 jump · Enter submit · Esc cancel ",
        Style::default().fg(theme.muted()),
    )]));

    let block = panel_block(theme.brand(), theme.panel());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

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
    let bg = if is_highlighted {
        theme.brand()
    } else {
        theme.panel()
    };
    let fg = if is_highlighted {
        contrast_fg(bg)
    } else {
        theme.fg()
    };
    let marker_style = Style::default()
        .bg(bg)
        .fg(if is_selected { theme.ok() } else { fg });
    let text_style = Style::default().bg(bg).fg(fg);
    let desc_style = Style::default().bg(bg).fg(theme.muted());

    let mut spans = vec![
        Span::styled(format!(" {:>2} ", number), text_style),
        Span::styled(format!("{} ", marker), marker_style),
        Span::styled(label.to_string(), text_style),
    ];
    if let Some(desc) = description {
        spans.push(Span::styled(" — ", desc_style));
        spans.push(Span::styled(desc.to_string(), desc_style));
    }
    lines.push(Line::from(vec![Span::styled(" ", text_style)]));
    lines.push(Line::from(spans));
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
    let marker_style = Style::default()
        .fg(theme.warn())
        .add_modifier(Modifier::BOLD);
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
