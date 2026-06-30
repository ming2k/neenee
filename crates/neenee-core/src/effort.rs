//! Reasoning-effort control — the per-model "how hard should I think before
//! answering" knob, provider-independent.
//!
//! This is the canonical home for [`Effort`] because effort is a **model
//! capability**, not a transport detail: which effort levels a model honors
//! (e.g. `xhigh` is Opus-4.7+/Fable only) is an intrinsic property of the
//! model, so it belongs next to [`crate::model::Model`] (which carries the
//! per-model `effort_levels` slice). Protocol layers (the Anthropic Messages
//! provider) translate a chosen [`Effort`] into their wire field
//! (`output_config.effort`); the chosen value can live on a channel as a user
//! *override*, but the *capability set* lives here.

/// How much reasoning effort a model should spend before answering.
///
/// A model accepts only a subset of these levels (its
/// [`crate::model::Model::effort_levels`]); callers must clamp a requested
/// level down to what the model supports rather than sending an unsupported
/// value (which the upstream rejects with 400).
///
/// Ordered ascending by depth: `Low < Medium < High < Xhigh < Max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// Minimal reasoning; simple tasks may skip thinking entirely. Fastest and
    /// cheapest. Useful for sub-agents and trivial classification.
    Low,
    /// Moderate reasoning. A middle ground that may omit thinking on simple
    /// queries.
    Medium,
    /// The default depth — deep reasoning on all but the most trivial tasks.
    /// Equivalent to omitting `effort` entirely.
    High,
    /// Deeper-than-high reasoning with extended exploration. Only the Fable /
    /// Opus-4.7+ tier supports it; the best setting for most coding and
    /// agentic work on those models.
    Xhigh,
    /// Maximum reasoning with no depth cap. Correctness over cost; use when a
    /// wrong answer is expensive.
    Max,
}

impl Effort {
    /// All levels in ascending order of depth.
    pub const ORDER: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    /// The wire string sent in the provider's effort field
    /// (`output_config.effort` for Anthropic).
    pub const fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// The level's position in [`Effort::ORDER`], for comparison/clamping.
    fn rank(self) -> usize {
        Self::ORDER.iter().position(|e| *e == self).unwrap_or(2)
    }

    /// Parse a lowercase effort string (`"low"`/`"medium"`/`"high"`/
    /// `"xhigh"`/`"max"`) into the typed [`Effort`]. Returns `None` for
    /// anything else so an unrecognized config value is silently ignored
    /// rather than treated as an error — the caller keeps its default.
    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::Xhigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }

    /// Clamp `self` down to the highest allowed level ≤ `self` (so a requested
    /// `xhigh` on a model that tops out at `high` becomes `high`, never an
    /// unsupported value). Falls back to `high` (the wire default) when nothing
    /// allowed ranks ≤ the request.
    pub fn clamp_to(self, allowed: &[Effort]) -> Effort {
        let req = self.rank();
        allowed
            .iter()
            .copied()
            .filter(|e| e.rank() <= req)
            .max_by_key(|e| e.rank())
            .unwrap_or(Effort::High)
    }
}

/// `low`/`medium`/`high` — the conservative effort set assumed for any model
/// whose higher tiers (`xhigh`/`max`) are not known (third-party
/// Anthropic-compatible relays serving non-Claude models). Sending an unknown
/// tier to such an upstream risks a 400, so the safe subset is the default.
pub const EFFORT_COMMON: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High];

/// The full `low..=max` range including `xhigh`, honored by the models that
/// accept every tier: Claude Opus 4.8 and Opus 4.7 (and Fable 5 / Mythos 5).
/// `xhigh` is *not* universal — Opus/Sonnet 4.6 reject it (use
/// [`EFFORT_CLAUDE_NO_XHIGH`]).
pub const EFFORT_CLAUDE_FULL: &[Effort] = &[
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];

/// `low`/`medium`/`high`/`max` — the effort range for Claude models that honor
/// `max` but **not** `xhigh`: Claude Sonnet 4.6 and Opus 4.6. (`xhigh` is
/// limited to Opus 4.8 / 4.7 and the Fable/Mythos line.) Requesting `xhigh`
/// here clamps down to `high`.
pub const EFFORT_CLAUDE_NO_XHIGH: &[Effort] =
    &[Effort::Low, Effort::Medium, Effort::High, Effort::Max];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips() {
        for e in Effort::ORDER {
            assert_eq!(Effort::parse(e.as_str()), Some(e));
        }
        assert_eq!(Effort::parse("nonsense"), None);
        assert_eq!(Effort::parse("  HIGH "), Some(Effort::High));
    }

    #[test]
    fn clamp_downgrades_unsupported_tier() {
        // xhigh on a model that tops out at high → high.
        assert_eq!(Effort::Xhigh.clamp_to(EFFORT_COMMON), Effort::High);
        // max on a full-tier model stays max.
        assert_eq!(Effort::Max.clamp_to(EFFORT_CLAUDE_FULL), Effort::Max);
        // low is honored everywhere.
        assert_eq!(Effort::Low.clamp_to(EFFORT_COMMON), Effort::Low);
    }
}
