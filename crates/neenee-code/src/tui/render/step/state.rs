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
    /// Keyboard focus ring is on this step. Reads as the **hover tone**
    /// via [`summary_weight`] (same color as a pointer hover), because focus
    /// is a transient "look here" cue — it shares hover's color rather than
    /// pinning the step to full brightness. Focus takes priority over hover
    /// only in [`from_hover_focused`](Interaction::from_hover_focused)'s
    /// enum resolution; both land on the same color in [`summary_weight`],
    /// so a focused step never silently darkens when the pointer leaves.
    Focused,
}

impl Interaction {
    /// Build from the raw interaction flags produced by the call site.
    ///
    /// Priority: **focus** beats **hover** beats **idle**. Focus wins over
    /// hover purely to keep the enum deterministic (both resolve to the same
    /// color in [`summary_weight`], so a focused step stays highlighted even
    /// after the pointer moves away).
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
/// **Three-tone, hover-priority model.** Each cause maps to its own distinct
/// color so the three states never collapse into one and stay easy to tell
/// apart at a glance:
///
/// 1. **Hover (highest priority)** → `theme.hover()`. Whether the step is open
///    or closed, the pointer resting on it is the strongest transient signal,
///    so it always reads as the intermediate hover tone. Keyboard focus is
///    folded into this same tone (focus is a transient "look here" cue, so it
///    shares hover's color rather than pinning the step to full brightness).
/// 2. **Expanded (idle)** → `theme.fg()`. An open body is the active state, so
///    it reads as the primary foreground.
/// 3. **Collapsed (idle)** → `theme.muted()`. A closed, idle step recedes.
///
/// Expanded and collapsed are **mutually exclusive peers** (same priority,
/// decided only when idle); hover overrides whichever one is active. This
/// means closing a step *immediately* darkens it to muted instead of staying
/// bright, because the close click no longer collides with a focus override.
pub fn summary_weight(disclosure: Disclosure, interaction: Interaction, theme: &Theme) -> Color {
    match interaction {
        Interaction::Hovered | Interaction::Focused => theme.hover(),
        Interaction::Idle => match disclosure {
            Disclosure::Expanded => theme.fg(),
            Disclosure::Collapsed => theme.muted(),
        },
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
/// factor (`accent_idle_blend`, `accent_hover_blend`, `accent_focus_blend`)
/// that mirrors [`summary_weight`]'s three-tone model: an idle collapsed step
/// leaves the accent essentially intact (the running step stays vivid), hover
/// / focus lean a little toward `theme.hover()`, and an open (expanded) body
/// leans toward the primary foreground so the active state reads clearly on
/// top of the hue.
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
    // so the hue dominates but hover / open-body / idle still produce a visible
    // luminance shift. The factor mirrors the three-tone weight ladder: hover
    // (and focus) lean toward the hover tone, an expanded body leans toward the
    // primary foreground, and an idle collapsed step leaves the accent intact.
    let weight = summary_weight(disclosure, interaction, theme);
    let t = accent_blend_factor(disclosure, interaction);
    accent.blend(weight, t)
}

/// How strongly a lifecycle accent yields to the disclosure × interaction
/// weight color. Kept small so the hue (running / failed / denied) stays the
/// dominant signal, but non-zero on the transient / active states so the
/// accent still visibly shifts toward the matching weight-ladder color. The
/// rungs mirror [`summary_weight`]'s three-tone, hover-priority model:
///
/// - Hover / focus → lean toward `theme.hover()` (the strongest transient
///   cue, regardless of disclosure).
/// - Expanded + idle → lean toward `theme.fg()` (an open body is active).
/// - Collapsed + idle → leave the accent intact (the running step stays vivid
///   while it recedes).
fn accent_blend_factor(disclosure: Disclosure, interaction: Interaction) -> f32 {
    match interaction {
        Interaction::Hovered | Interaction::Focused => ACCENT_HOVER_BLEND,
        Interaction::Idle => match disclosure {
            Disclosure::Expanded => ACCENT_FOCUS_BLEND,
            Disclosure::Collapsed => ACCENT_IDLE_BLEND,
        },
    }
}

/// Blend factors used to compose a lifecycle accent with the weight ladder.
/// Exposed as module consts so the unit tests assert the exact composed color
/// rather than only "it changed". Hover/focus share one rung (both are
/// transient "look here" cues), expanded is its own rung (the active state),
/// and idle-collapsed leaves the accent essentially untouched.
const ACCENT_IDLE_BLEND: f32 = 0.0;
const ACCENT_HOVER_BLEND: f32 = 0.35;
const ACCENT_FOCUS_BLEND: f32 = 0.6;

#[cfg(test)]
mod tests {
    use super::*;

    /// The three tones are always distinct, so a step can never ambiguously
    /// share a color between two causes. This is the core invariant of the
    /// hover-priority model: hover ≠ fg ≠ muted.
    #[test]
    fn three_tones_are_distinct() {
        let theme = Theme::default();
        assert_ne!(theme.hover(), theme.fg());
        assert_ne!(theme.hover(), theme.muted());
        assert_ne!(theme.fg(), theme.muted());
    }

    /// Hover is the highest priority: regardless of disclosure, a hovered step
    /// reads as the intermediate hover tone — never the expanded fg or the idle
    /// muted. This is what lets hover stay a distinct third color.
    #[test]
    fn hover_dominates_disclosure() {
        let theme = Theme::default();
        assert_eq!(
            summary_weight(Disclosure::Collapsed, Interaction::Hovered, &theme),
            theme.hover()
        );
        assert_eq!(
            summary_weight(Disclosure::Expanded, Interaction::Hovered, &theme),
            theme.hover()
        );
    }

    /// Keyboard focus shares the hover tone (a transient "look here" cue). This
    /// keeps the three-tone model intact — focus does not introduce a fourth
    /// color — while still standing out from muted idle siblings.
    #[test]
    fn focused_reads_as_hover_tone() {
        let theme = Theme::default();
        assert_eq!(
            summary_weight(Disclosure::Collapsed, Interaction::Focused, &theme),
            theme.hover()
        );
        assert_eq!(
            summary_weight(Disclosure::Expanded, Interaction::Focused, &theme),
            theme.hover()
        );
        // Distinct from both the idle states.
        assert_ne!(
            summary_weight(Disclosure::Collapsed, Interaction::Focused, &theme),
            theme.muted()
        );
    }

    /// Expanded and collapsed are mutually exclusive peers, decided only when
    /// idle: an open idle step is the primary foreground, a closed idle step is
    /// muted. This is the regression for the original bug — closing a step must
    /// *immediately* darken it instead of staying bright.
    #[test]
    fn idle_disclosure_decides_fg_vs_muted() {
        let theme = Theme::default();
        assert_eq!(
            summary_weight(Disclosure::Expanded, Interaction::Idle, &theme),
            theme.fg()
        );
        assert_eq!(
            summary_weight(Disclosure::Collapsed, Interaction::Idle, &theme),
            theme.muted()
        );
    }

    /// Regression for the reported bug: after clicking a summary to collapse
    /// it, the step must darken to muted. The close click also sets keyboard
    /// focus, but under the new model focused collapses to the hover tone —
    /// still distinct from the expanded fg — and once the pointer/focus leaves
    /// it reads as muted. An expanded step is therefore brighter than a closed
    /// one in every non-hover state.
    #[test]
    fn closing_a_step_darkens_it() {
        let theme = Theme::default();
        let open = summary_weight(Disclosure::Expanded, Interaction::Idle, &theme);
        let closed = summary_weight(Disclosure::Collapsed, Interaction::Idle, &theme);
        assert_ne!(
            open, closed,
            "an open step must not read the same color as a closed idle one"
        );
        // fg is brighter than muted in the default theme, so "darkens" holds.
        assert_ne!(open, theme.muted());
        assert_eq!(closed, theme.muted());
    }

    /// A lifecycle accent is *not* discarded: idle + accent returns the accent
    /// untouched (the running / failed step stays vivid), while the weight
    /// channel still nudges the brightness on hover / focus / open. This is the
    /// composition contract — the hue dominates, the luminance shifts.
    #[test]
    fn accent_idle_is_intact_hover_focus_blend() {
        let theme = Theme::default();
        let accent = Color::Rgb(128, 153, 156); // an arbitrary accent (e.g. info hue)
        // Idle collapsed: the accent is returned unchanged.
        assert_eq!(
            summary_text_color(Some(accent), Disclosure::Collapsed, Interaction::Idle, &theme),
            accent
        );
        // Hover (collapsed): the accent leans toward theme.hover() but keeps its
        // hue, so it is distinct from both the idle accent and the plain hover.
        let hovered =
            summary_text_color(Some(accent), Disclosure::Collapsed, Interaction::Hovered, &theme);
        assert_ne!(hovered, accent, "hover must visibly shift an accent step");
        assert_ne!(hovered, theme.hover());
        assert_eq!(hovered, accent.blend(theme.hover(), ACCENT_HOVER_BLEND));
        // Focus shares the hover rung.
        let focused =
            summary_text_color(Some(accent), Disclosure::Collapsed, Interaction::Focused, &theme);
        assert_eq!(focused, accent.blend(theme.hover(), ACCENT_HOVER_BLEND));
        // Expanded + idle leans toward the primary foreground (its own rung).
        let expanded =
            summary_text_color(Some(accent), Disclosure::Expanded, Interaction::Idle, &theme);
        assert_ne!(expanded, accent);
        assert_eq!(expanded, accent.blend(theme.fg(), ACCENT_FOCUS_BLEND));
    }

    /// Regression: an accent step must shift on hover. Before the fix a running
    /// subagent (`explore`) pinned the summary to a flat accent for its whole
    /// lifetime and hovering did nothing. The composed result must differ
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
            "hovering an accented step must change its color"
        );
    }

    /// No accent falls through to the weight ladder (the three-tone model).
    #[test]
    fn no_accent_uses_weight() {
        let theme = Theme::default();
        // Idle peers: expanded → fg, collapsed → muted.
        assert_eq!(
            summary_text_color(None, Disclosure::Expanded, Interaction::Idle, &theme),
            theme.fg()
        );
        assert_eq!(
            summary_text_color(None, Disclosure::Collapsed, Interaction::Idle, &theme),
            theme.muted()
        );
        // Hover dominates disclosure.
        assert_eq!(
            summary_text_color(None, Disclosure::Collapsed, Interaction::Hovered, &theme),
            theme.hover()
        );
        assert_eq!(
            summary_text_color(None, Disclosure::Expanded, Interaction::Hovered, &theme),
            theme.hover()
        );
        // Focus shares the hover tone.
        assert_eq!(
            summary_text_color(None, Disclosure::Collapsed, Interaction::Focused, &theme),
            theme.hover()
        );
    }

    /// `from_hover_focused` priority: focus > hover > idle. The enum keeps
    /// focus distinct from hover for determinism, even though both resolve to
    /// the same color in `summary_weight` — so a focused step stays highlighted
    /// after the pointer leaves.
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
