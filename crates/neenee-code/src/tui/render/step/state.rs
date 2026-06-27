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
/// - A **non-completed lifecycle** supplies an accent (hue) which stays in
///   force so a running / failed / denied step remains visibly accented even
///   when collapsed and idle. The caller computes the accent from its
///   kind-specific lifecycle source (e.g. `ToolStatus::color`); per ADR 0008
///   the accent is a steady hue, never a breathing sweep — the activity bar
///   owns the only motion in the TUI.
/// - `None` (completed, or a kind whose lifecycle only affects its marker —
///   reasoning) hands control fully to [`summary_weight`].
///
/// The two channels compose: when an accent is present it supplies the **hue**
/// while the disclosure × interaction weight channel still modulates the
/// **brightness**. Without that composition a long-lived accent — like the
/// `info` hue on a running subagent task — would pin the summary to one flat
/// color for its whole lifetime and the hover/focus affordance would never
/// show, which is exactly the bug where hovering an `explore` step did
/// nothing. The accent is nudged toward the weight-ladder color by a per-rung
/// factor (`accent_idle_blend`, `accent_hover_blend`, `accent_focus_blend`):
/// idle leaves the accent essentially intact (the running step stays vivid),
/// hover leans a little toward `theme.hover()`, and focus / an open body lean
/// toward the primary foreground so the deliberate cue reads clearly on top of
/// the hue.
///
/// This is the single entry point renderers use for the summary text color,
/// keeping the accent/weight separation in one auditable place.
pub fn summary_text_color(
    accent: Option<Color>,
    disclosure: Disclosure,
    interaction: Interaction,
    theme: &Theme,
) -> Color {
    let Some(accent) = accent else {
        return summary_weight(disclosure, interaction, theme);
    };
    // The weight-ladder color for this disclosure × interaction gives the
    // brightness target. Blend the accent toward it by a rung-specific factor
    // so the hue dominates but hover / focus still produce a visible shift.
    let weight = summary_weight(disclosure, interaction, theme);
    let t = accent_blend_factor(disclosure, interaction);
    accent.blend(weight, t)
}

/// How strongly a lifecycle accent yields to the disclosure × interaction
/// weight color. Kept small so the hue (running / failed / denied) stays the
/// dominant signal: idle leaves the accent untouched, hover adds a gentle
/// nudge, and focus / an open body push it harder toward the primary
/// foreground. `Expanded` reads as the active (focused) state since an open
/// body is the strongest signal regardless of interaction, mirroring
/// [`summary_weight`].
fn accent_blend_factor(disclosure: Disclosure, interaction: Interaction) -> f32 {
    match (disclosure, interaction) {
        (Disclosure::Expanded, _) | (Disclosure::Collapsed, Interaction::Focused) => {
            ACCENT_FOCUS_BLEND
        }
        (Disclosure::Collapsed, Interaction::Hovered) => ACCENT_HOVER_BLEND,
        (Disclosure::Collapsed, Interaction::Idle) => ACCENT_IDLE_BLEND,
    }
}

/// Blend factors used to compose a lifecycle accent with the weight ladder.
/// Exposed as module consts so the unit tests assert the exact composed color
/// rather than only "it changed".
const ACCENT_IDLE_BLEND: f32 = 0.0;
const ACCENT_HOVER_BLEND: f32 = 0.35;
const ACCENT_FOCUS_BLEND: f32 = 0.6;

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

    /// A lifecycle accent is *not* discarded: idle + accent returns the accent
    /// untouched (the running / failed step stays vivid), while the weight
    /// channel still nudges the brightness on hover / focus. This is the
    /// composition contract — the hue dominates, the luminance shifts.
    #[test]
    fn accent_idle_is_intact_hover_focus_blend() {
        let theme = Theme::default();
        let accent = Color::Rgb(128, 153, 156); // matches theme.info (a running step)
        // Idle: the accent is returned unchanged.
        assert_eq!(
            summary_text_color(Some(accent), Disclosure::Collapsed, Interaction::Idle, &theme),
            accent
        );
        // Hover: the accent leans toward theme.hover() but keeps its hue, so it
        // is distinct from both the idle accent and the plain hover tone.
        let hovered =
            summary_text_color(Some(accent), Disclosure::Collapsed, Interaction::Hovered, &theme);
        assert_ne!(hovered, accent, "hover must visibly shift an accent step");
        assert_ne!(hovered, theme.hover());
        assert_eq!(
            hovered,
            accent.blend(theme.hover(), ACCENT_HOVER_BLEND)
        );
        // Focus / expanded: leans harder toward the primary foreground.
        let focused =
            summary_text_color(Some(accent), Disclosure::Collapsed, Interaction::Focused, &theme);
        assert_ne!(focused, accent);
        assert_eq!(focused, accent.blend(theme.fg(), ACCENT_FOCUS_BLEND));
        let expanded =
            summary_text_color(Some(accent), Disclosure::Expanded, Interaction::Idle, &theme);
        assert_eq!(expanded, accent.blend(theme.fg(), ACCENT_FOCUS_BLEND));
    }

    /// Regression: an accent step must brighten on hover. Before the fix a
    /// running subagent (`explore`) pinned the summary to a flat accent for its
    /// whole lifetime and hovering did nothing, because the accent won outright
    /// and the weight channel was bypassed. The composed result must differ
    /// between idle and hover.
    #[test]
    fn accent_step_hover_is_visible() {
        let theme = Theme::default();
        let accent = theme.info();
        let idle = summary_text_color(
            Some(accent),
            Disclosure::Collapsed,
            Interaction::Idle,
            &theme,
        );
        let hover = summary_text_color(
            Some(accent),
            Disclosure::Collapsed,
            Interaction::Hovered,
            &theme,
        );
        assert_ne!(
            idle, hover,
            "hovering an accent (running) step must change its color"
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
