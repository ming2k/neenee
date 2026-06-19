//! Step state machine: the three orthogonal axes that determine a step's
//! presentation, and the pure functions that reduce them to color.
//!
//! See [`super`] for the full architectural overview; this module owns the
//! state types and the accent/weight resolution so they can be unit-tested in
//! isolation from rendering.

use ratatui::style::Color;

use super::Theme;

/// Whether a step's body is shown. User-controlled (click / `Enter` /
/// auto-expand on first stream chunk) and persisted on the message so it
/// survives redraws and history restore.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disclosure {
    /// Only the one-line summary is visible.
    Collapsed,
    /// The summary plus its body are both visible.
    Expanded,
}

impl Disclosure {
    /// Build from the raw `expanded` bool carried on the message.
    pub fn from_expanded(expanded: bool) -> Self {
        if expanded {
            Disclosure::Expanded
        } else {
            Disclosure::Collapsed
        }
    }
}

/// Transient interaction with a step summary, recomputed every frame from
/// pointer / keyboard state. Never persisted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interaction {
    /// Not under the pointer and not keyboard-focused.
    Idle,
    /// Pointer rests on the summary line — a soft hover affordance.
    Hovered,
    /// Keyboard focus ring is on this step. Deliberately does **not** brighten
    /// the summary (see [`summary_weight`]): focus is a separate concern from
    /// disclosure/weight and is conveyed by its own cue (e.g. a marker tint or
    /// rail), never by stealing the "open/hover" luminance channel. This is
    /// what prevents a collapsed, focused step from reading as "still
    /// highlighted".
    ///
    /// Not yet constructed by any caller — kept in the contract so
    /// [`summary_weight`]'s match stays exhaustive and the future focus-ring
    /// cue has a first-class state to read.
    #[allow(dead_code)]
    Focused,
}

impl Interaction {
    /// Build from the raw `hovered` flag produced by the pointer hit-test.
    /// Keyboard focus is tracked separately at the call site and currently does
    /// not feed the weight channel.
    pub fn from_hover(hovered: bool) -> Self {
        if hovered {
            Interaction::Hovered
        } else {
            Interaction::Idle
        }
    }
}

/// Summary-line **weight** (luminance) — a pure function of disclosure ×
/// interaction. This is the "is it open / am I pointing at it?" channel only;
/// it never depends on lifecycle, so it cannot leak run-state or focus into the
/// brightness.
///
/// Priority:
/// - An **expanded** step reads as the primary foreground (the active state)
///   regardless of interaction — its body being open is the strongest signal.
/// - A **collapsed** step under the pointer lights up to the intermediate hover
///   tone as a click affordance.
/// - Otherwise (collapsed + idle, or collapsed + focused) it stays muted.
pub fn summary_weight(disclosure: Disclosure, interaction: Interaction, theme: &Theme) -> Color {
    match (disclosure, interaction) {
        (Disclosure::Expanded, _) => theme.fg(),
        (Disclosure::Collapsed, Interaction::Hovered) => theme.hover(),
        (Disclosure::Collapsed, _) => theme.muted(),
    }
}

/// Resolve the final summary text color from both channels:
///
/// - A **non-completed lifecycle** supplies an accent (hue) which wins outright
///   so a running / failed / denied step stays visibly accented even when
///   collapsed and idle. The caller computes the accent from its kind-specific
///   lifecycle source (e.g. `ToolStatus::color`, with breathing applied for
///   `Running`).
/// - `None` (completed, or a kind whose lifecycle only affects its marker —
///   reasoning) hands control to [`summary_weight`].
///
/// This is the single entry point renderers use for the summary text color,
/// keeping the accent/weight separation in one auditable place.
pub fn summary_text_color(
    accent: Option<Color>,
    disclosure: Disclosure,
    interaction: Interaction,
    theme: &Theme,
) -> Color {
    match accent {
        Some(color) => color,
        None => summary_weight(disclosure, interaction, theme),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expanded dominates every interaction state — an open step is always the
    /// active tone, never muted by being idle.
    #[test]
    fn expanded_dominates_interaction() {
        let theme = Theme::default();
        assert_eq!(
            summary_weight(Disclosure::Expanded, Interaction::Idle, &theme),
            theme.fg()
        );
        assert_eq!(
            summary_weight(Disclosure::Expanded, Interaction::Hovered, &theme),
            theme.fg()
        );
        assert_eq!(
            summary_weight(Disclosure::Expanded, Interaction::Focused, &theme),
            theme.fg()
        );
    }

    /// Collapsed + hovered is the intermediate hover tone — distinct from both
    /// the expanded (fg) and idle (muted) states.
    #[test]
    fn collapsed_hovered_is_intermediate() {
        let theme = Theme::default();
        let hovered = summary_weight(Disclosure::Collapsed, Interaction::Hovered, &theme);
        assert_eq!(hovered, theme.hover());
        assert_ne!(hovered, theme.fg());
        assert_ne!(hovered, theme.muted());
    }

    /// Collapsed + focused collapses to muted — the regression guard for the
    /// "collapsed focused step stays highlighted" bug.
    #[test]
    fn collapsed_focused_is_muted() {
        let theme = Theme::default();
        assert_eq!(
            summary_weight(Disclosure::Collapsed, Interaction::Focused, &theme),
            theme.muted()
        );
    }

    /// A lifecycle accent overrides the weight channel entirely.
    #[test]
    fn accent_overrides_weight() {
        let theme = Theme::default();
        let accent = Color::Rgb(255, 0, 0);
        assert_eq!(
            summary_text_color(
                Some(accent),
                Disclosure::Collapsed,
                Interaction::Idle,
                &theme
            ),
            accent
        );
        assert_eq!(
            summary_text_color(
                Some(accent),
                Disclosure::Expanded,
                Interaction::Hovered,
                &theme
            ),
            accent
        );
    }

    /// No accent falls through to the weight ladder.
    #[test]
    fn no_accent_uses_weight() {
        let theme = Theme::default();
        assert_eq!(
            summary_text_color(None, Disclosure::Expanded, Interaction::Idle, &theme),
            theme.fg()
        );
        assert_eq!(
            summary_text_color(None, Disclosure::Collapsed, Interaction::Hovered, &theme),
            theme.hover()
        );
        assert_eq!(
            summary_text_color(None, Disclosure::Collapsed, Interaction::Idle, &theme),
            theme.muted()
        );
    }
}
