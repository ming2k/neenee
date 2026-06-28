//! Empty-state hero shown in place of the transcript when a session holds no
//! messages yet.
//!
//! This is a **replacement** for the transcript stream, not content rendered
//! *inside* it: `draw_transcript` short-circuits to this component when
//! `messages` is empty (and no envoy/side view is open), keeping the empty
//! state out of the message-rendering pipeline entirely. Responsibilities stay
//! clean — the empty state never participates in scroll, selection, or
//! attribution logic.
//!
//! The footer (input box, status bar, hint bar) renders exactly as in a live
//! session, so the user lands in a familiar composer immediately.
//!
//! The logo source is pluggable: a caller may pass user-supplied lines (loaded
//! from `$XDG_CONFIG_HOME/neenee/logo.txt`); when absent the built-in figlet
//! wordmark is used. Either way the art is clamped to a safe bounding box so a
//! giant paste can never blow out the welcome screen.

use neenee_tui::{
    Alignment, Frame, Paragraph, Rect, {Line, Span}, {Modifier, Style},
};

use super::theme::Theme;

/// Hard width cap (in terminal columns) for any logo line. A wider line is
/// truncated at a character boundary. 60 keeps comfortable side margins inside
/// an 80-column terminal while leaving room for the tagline beneath.
pub(super) const MAX_LOGO_COLS: usize = 60;

/// Hard height cap (in rows) for the logo block. More lines than this are
/// dropped from the bottom. 20 leaves the welcome screen readable on a
/// 24-row terminal even before vertical centering.
pub(super) const MAX_LOGO_ROWS: usize = 20;

/// The built-in wordmark, rendered when no user logo is supplied (figlet
/// "small" font). Compact enough to fit an 80-column terminal with room for the
/// tagline beneath, while still reading as a logo at a glance rather than
/// competing with the transcript that will replace it.
const BUILTIN_LOGO: &[&str] = &[
    " _ _  ___ ___ _ _  ___ ___ ",
    "| ' \\/ -_) -_) ' \\/ -_) -_|",
    "|_||_\\___\\___|_||_\\___\\___|",
];

/// Parse a raw logo file into a display-safe line vector, enforcing the
/// [`MAX_LOGO_COLS`] × [`MAX_LOGO_ROWS`] bounding box:
///
/// - Lines are split on `\n`; trailing `\r` (CRLF files) is stripped.
/// - Each line is truncated to `MAX_LOGO_COLS` chars.
/// - Leading/trailing blank lines are dropped (so a trailing newline doesn't
///   waste a centered row).
/// - At most `MAX_LOGO_ROWS` lines are kept (excess dropped from the bottom).
///
/// Returns `None` when the input yields no visible lines, so the caller falls
/// back to the built-in logo rather than rendering nothing.
pub(crate) fn parse_logo(raw: &str) -> Option<Vec<String>> {
    let mut lines: Vec<String> = raw
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .map(|l| truncate_chars(l, MAX_LOGO_COLS))
        .skip_while(|l| l.trim().is_empty())
        .collect();
    // Trim trailing blank lines.
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.truncate(MAX_LOGO_ROWS);
    if lines.is_empty() { None } else { Some(lines) }
}

/// Truncate a string to at most `max` display chars (by Unicode scalar value,
/// not graphemes — terminals are cell-oriented and ASCII art is the target
/// use case). Does not split multi-byte chars.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Compute the height the empty state occupies for a given logo, without
/// drawing. Lets the transcript renderer keep its `content_lines` accounting
/// honest so the app loop does not treat an empty session as a zero-height
/// stream. `logo` is the effective lines (user-supplied or built-in).
fn empty_state_height(logo: &[&str]) -> usize {
    logo.len() + 2 // logo rows + blank gap before the tagline
}

/// Resolve the effective logo lines: user-supplied lines when present,
/// otherwise the built-in wordmark.
fn effective_logo(user_logo: Option<&[String]>) -> Vec<&str> {
    if let Some(lines) = user_logo {
        if !lines.is_empty() {
            return lines.iter().map(String::as_str).collect();
        }
    }
    BUILTIN_LOGO.to_vec()
}

/// Draw the empty-state hero centered in `area`. Paints nothing outside the
/// given rect.
///
/// `user_logo` — when `Some` and non-empty, replaces the built-in wordmark.
/// The caller is responsible for having loaded + parsed it (clamped here as a
/// safety net regardless).
pub(super) fn draw_empty_state(
    frame: &mut Frame,
    area: Rect,
    user_logo: Option<&[String]>,
    theme: &Theme,
) {
    // If the user logo somehow slipped through un-clamped, clamp it here too so
    // rendering stays within bounds even if a caller bypassed `parse_logo`.
    let user_clamped: Option<Vec<String>> =
        user_logo.map(|lines| parse_logo(&lines.join("\n")).unwrap_or_default());
    let logo_refs: Vec<&str> = if let Some(ref clamped) = user_clamped {
        if !clamped.is_empty() {
            clamped.iter().map(String::as_str).collect()
        } else {
            BUILTIN_LOGO.to_vec()
        }
    } else {
        BUILTIN_LOGO.to_vec()
    };

    let logo_fg = theme.brand();
    let logo_style = Style::default().fg(logo_fg).add_modifier(Modifier::BOLD);
    let tagline_style = Style::default().fg(theme.muted());

    let mut lines: Vec<Line> = Vec::with_capacity(logo_refs.len() + 3);
    for row in &logo_refs {
        lines.push(Line::from(vec![Span::styled(*row, logo_style)]));
    }
    // Blank gap, then a one-line invitation.
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "Type a message below to begin.",
        tagline_style,
    )]));

    // Center vertically: push the whole block down by half the slack so it sits
    // roughly in the middle of the viewport rather than pinned to the top.
    let slack = area.height.saturating_sub(lines.len() as u16) / 2;
    let top = area.y + slack;

    let para = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(
        para,
        Rect::new(area.x, top, area.width, area.height - slack),
    );
}

/// Height the empty state reports for `content_lines` accounting. Uses the
/// user logo's line count when supplied (clamped), else the built-in height.
pub(super) fn empty_state_content_lines(user_logo: Option<&[String]>) -> usize {
    let refs = effective_logo(user_logo);
    empty_state_height(&refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state_renders_builtin_without_panicking() {
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let theme = Theme::default();
        terminal.draw(|f| {
            draw_empty_state(f, f.area(), None, &theme);
        });
    }

    #[test]
    fn empty_state_renders_user_logo_without_panicking() {
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let theme = Theme::default();
        let logo = vec!["  X X  ".to_string(), " X X X ".to_string()];
        terminal.draw(|f| {
            draw_empty_state(f, f.area(), Some(&logo), &theme);
        });
    }

    #[test]
    fn parse_logo_truncates_wide_lines() {
        let wide = "a".repeat(200);
        let out = parse_logo(&wide).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), MAX_LOGO_COLS);
    }

    #[test]
    fn parse_logo_truncates_tall_blocks() {
        let tall = (0..MAX_LOGO_ROWS + 50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = parse_logo(&tall).unwrap();
        assert_eq!(out.len(), MAX_LOGO_ROWS);
    }

    #[test]
    fn parse_logo_strips_crlf_and_trailing_blanks() {
        let raw = "\r\nhello\r\nworld\r\n\r\n";
        let out = parse_logo(raw).unwrap();
        assert_eq!(out, vec!["hello", "world"]);
    }

    #[test]
    fn parse_logo_returns_none_for_empty_input() {
        assert!(parse_logo("").is_none());
        assert!(parse_logo("\n\n\n").is_none());
        assert!(parse_logo("   \n  \n").is_none());
    }

    #[test]
    fn builtin_height_matches_logo_plus_gap() {
        let refs = effective_logo(None);
        assert_eq!(empty_state_height(&refs), BUILTIN_LOGO.len() + 2);
    }

    #[test]
    fn content_lines_reflects_user_logo_size() {
        let user = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(empty_state_content_lines(Some(&user)), 5); // 3 + 2
        assert_eq!(empty_state_content_lines(None), BUILTIN_LOGO.len() + 2);
    }
}
