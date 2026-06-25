//! Provider picker and API-key / model-id editor modals.

use std::collections::HashMap;

use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::layout::LayoutMap;
use neenee_core::ProviderPickerSnapshot;

use super::common::caret_column;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    centered_rect, contrast_fg, modal_frame, render_body, viewport_rect,
};

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
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Draw the second-stage model picker for a multi-model provider (opencode-go).
/// Lists the provider's models with their display names; the highlighted row is
/// activated on `Enter`. Esc returns to the provider picker.
pub(crate) fn draw_model_picker(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    solution: &crate::tui::ProviderPreset,
    current_model: &str,
    modal_index: usize,
    theme: &Theme,
) {
    let area = centered_rect(64, 60, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

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
                    format!("{} — pick a model", solution.name),
                    Style::default().fg(theme.muted()),
                ),
            ])),
            h,
        );
    }

    let mut body: Vec<Line> = Vec::new();
    for (row, &mid) in solution.models.iter().enumerate() {
        let is_current = mid == current_model;
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
        let dot = if is_current { "● " } else { "  " };
        let display = crate::tui::providers::model_display_name(mid);
        body.push(Line::from(vec![
            Span::styled(dot.to_string(), dim),
            Span::styled(
                format!("{:<18} ", display),
                base.add_modifier(Modifier::BOLD),
            ),
            Span::styled(mid.to_string(), dim),
        ]));
    }
    render_body(frame, f.body, body, &mut 0, Some(modal_index));

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓ navigate · enter activate · esc back",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
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
    frame.set_cursor_position((cursor_x, cursor_y));
}
