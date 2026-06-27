//! Full-output detail overlay for a focused tool step (ADR-0001 Step 8).
//!
//! Shows the step's complete output in a centered, scrollable panel so a long
//! result can be inspected without scrolling the whole transcript. Shell output
//! is broken into `$ command`, stdout, stderr (in `error_fg`), and an exit
//! footer.

use neenee_tui::{
    Clear, Frame, Modifier, Paragraph, Span, {Line, Style},
};

use crate::tui::Modal;
use crate::tui::document::TranscriptMessage;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{modal_area, panel_block, panel_inner};

pub fn draw_tool_step_detail_overlay(
    frame: &mut Frame,
    msg: &TranscriptMessage,
    scroll: u16,
    theme: &Theme,
) -> neenee_tui::Rect {
    use crate::tui::document::MessageKind;
    let area =
        modal_area(frame, Modal::ToolStepDetail).expect("tool detail modal has fixed geometry");
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

    // Paint the panel chrome (bg + brand `┃` left bar) bare, then render the
    // content into `panel_inner` so a long line reserves the bar's mirrored
    // right gutter instead of running into the panel's right edge — the same
    // symmetric-inset contract the permission sheet and `modal_frame` use.
    frame.render_widget(panel_block(theme.brand(), theme.panel()), area);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll(scroll, 0)
            .wrap(neenee_tui::Wrap { trim: false }),
        panel_inner(area),
    );
    area
}
