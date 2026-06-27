//! Step state machine: the three orthogonal axes that determine a step's
//! presentation, and the pure functions that reduce them to color.
//!
//! See [`super`] for the full architectural overview; this module owns the
//! state types and the accent/weight resolution so they can be unit-tested in
//! isolation from rendering.

use neenee_tui::Color;

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
    /// Keyboard focus ring is on this step. Reads as the **primary foreground**
    /// via [`summary_weight`] (same luminance as an expanded step), so a focused
    /// step clearly stands out from its idle/muted siblings. Focus takes
    /// priority over hover in [`from_hover_focused`](Interaction::from_hover_focused)
    /// so the deliberate keyboard cue never yields to the incidental pointer
    /// affordance.
    Focused,
}

impl Interaction {
    /// Build from the raw interaction flags produced by the call site.
    ///
    /// Priority: **focus** beats **hover** beats **idle**. Focus wins over
    /// hover so a keyboard-navigating user never sees the pointer's soft
    /// affordance compete with the deliberate focus cue.
    pub fn from_hover_focused(hovered: bool, focused: bool) -> Self {
        if focused {
            Interaction::Focused
        } else if hovered {
            Interaction::Hovered
        } else {
            Interaction::Idle
        }
    }
}

/// Summary-line **weight** (luminance) — a pure function of disclosure ×
/// interaction. This is the "is it open / focused / under the pointer?"
/// channel only; it never depends on lifecycle, so it cannot leak run-state
/// into the brightness.
///
/// Priority:
/// - An **expanded** step reads as the primary foreground (the active state)
///   regardless of interaction — its body being open is the strongest signal.
/// - A **collapsed** step carrying keyboard focus also reads as the primary
///   foreground, so the user can tell at a glance which step the cursor is on.
/// - A **collapsed** step under the pointer (but not focused) lights up to the
///   intermediate hover tone as a soft click affordance.
/// - Otherwise (collapsed + idle) it stays muted.
pub fn summary_weight(disclosure: Disclosure, interaction: Interaction, theme: &Theme) -> Color {
    match (disclosure, interaction) {
        (Disclosure::Expanded, _) => theme.fg(),
        (Disclosure::Collapsed, Interaction::Focused) => theme.fg(),
        (Disclosure::Collapsed, Interaction::Hovered) => theme.hover(),
        (Disclosure::Collapsed, Interaction::Idle) => theme.muted(),
    }
}

/// Resolve the final summary text color from both channels:
///
/// - A **non-completed lifecycle** supplies an accent (hue) which wins outright
///   so a running / failed / denied step stays visibly accented even when
///   collapsed and idle. The caller computes the accent from its kind-specific
///   lifecycle source (e.g. `ToolStatus::color`); per ADR 0008 the accent is a
///   steady hue, never a breathing sweep — the activity bar owns the only
///   motion in the TUI.
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

    /// Collapsed + focused reads as the primary foreground — the core focus
    /// affordance. A keyboard-focused step must stand out from its muted idle
    /// siblings so the user can see exactly which step the cursor is on.
    #[test]
    fn collapsed_focused_is_primary() {
        let theme = Theme::default();
        let focused = summary_weight(Disclosure::Collapsed, Interaction::Focused, &theme);
        assert_eq!(focused, theme.fg());
        assert_ne!(focused, theme.muted(), "focused must not read as idle");
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
        // Collapsed + focused (no accent) → primary foreground.
        assert_eq!(
            summary_text_color(None, Disclosure::Collapsed, Interaction::Focused, &theme),
            theme.fg()
        );
    }

    /// `from_hover_focused` priority: focus > hover > idle. A focused step is
    /// always `Focused` even when also under the pointer, so the deliberate
    /// keyboard cue never yields to the incidental hover affordance.
    #[test]
    fn focus_beats_hover_beats_idle() {
        assert_eq!(
            Interaction::from_hover_focused(false, false),
            Interaction::Idle
        );
        assert_eq!(
            Interaction::from_hover_focused(true, false),
            Interaction::Hovered
        );
        assert_eq!(
            Interaction::from_hover_focused(false, true),
            Interaction::Focused
        );
        assert_eq!(
            Interaction::from_hover_focused(true, true),
            Interaction::Focused
        );
    }
}
