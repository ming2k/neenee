//! Unified renderer for harness-level notices (errors, turn-pause signals,
//! status summaries).
//!
//! Replaces the ad-hoc `TranscriptMessage::new(Role::System, format!("Error: …"))`
//! pattern that left every notice indistinguishable from any other system
//! message and forced consumers to string-sniff `"Error:"` prefixes to recover
//! severity. The [`NoticeSeverity`] → color/icon mapping lives here as the
//! single source of truth, so adding a new severity (or retuning its color)
//! touches one match arm instead of scattered call sites.
//!
//! [`NoticeSeverity`]: crate::tui::document::NoticeSeverity

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::document::{MessageKind, NoticeSeverity, TranscriptMessage};

use super::text_layout::wrap_text;
use super::{Theme, TRANSCRIPT_BODY_RIGHT_INSET};

/// Severity presentation: the leading glyph and its color.
///
/// Centralizing this here is the whole point of the notice component —
/// `ToolStatus::color` (in `render/tools/mod.rs`) owns the *tool-step* status
/// palette, while this owns the *transcript-notice* palette, so the two stay
/// consistent by construction (both read from `Theme`) without one depending
/// on the other.
fn severity_presentation(severity: NoticeSeverity, theme: &Theme) -> (&'static str, Color) {
    match severity {
        NoticeSeverity::Error => ("✖", theme.err()),
        NoticeSeverity::TurnLimit => ("⏸", theme.warn()),
        NoticeSeverity::Info => ("ℹ", theme.info()),
    }
}

/// Render a notice message: a severity-colored glyph followed by the notice
/// text, wrapped to the transcript body width. Mirrors the row-accounting
/// contract of `draw_message_body` (`skip_rows` / `current_y` /
/// `content_lines`) so it drops into the same per-message render loop without
/// special-casing.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_notice(
    frame: &mut Frame,
    area: Rect,
    msg: &TranscriptMessage,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    theme: &Theme,
) {
    let severity = match &msg.kind {
        MessageKind::Notice { severity } => *severity,
        _ => return,
    };
    let (glyph, color) = severity_presentation(severity, theme);

    // 3-col prefix: ` <glyph> ` so the glyph clears the left margin and has
    // breathing room before the text — matches the prose indent used by
    // `Block::Text` so notices align with body text rather than hugging the
    // gutter.
    let prefix_cols = 3usize;
    let prefix = format!(" {glyph} ");
    let body_wrap_width = area
        .width
        .saturating_sub(TRANSCRIPT_BODY_RIGHT_INSET + prefix_cols as u16)
        .max(1) as usize;

    let lines = wrap_text(&msg.raw, body_wrap_width);
    *content_lines += lines.len();

    let base = Style::default().fg(color);
    let glyph_style = Style::default().fg(color);
    for (idx, wl) in lines.iter().enumerate() {
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= area.y + area.height {
            break;
        }

        let line_prefix = if idx == 0 {
            prefix.clone()
        } else {
            " ".repeat(prefix_cols)
        };
        let line = Line::from(vec![
            Span::styled(line_prefix, glyph_style),
            Span::styled(wl.text.clone(), base),
        ]);
        let line_rect = Rect::new(area.x, *current_y, area.width, 1);
        frame.render_widget(Paragraph::new(line), line_rect);
        *current_y += 1;
    }
}
