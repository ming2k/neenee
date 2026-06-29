//! Extended-thinking control — the *on/off* knob for model reasoning, the
//! companion to [`crate::effort::Effort`] (which controls *depth*).
//!
//! This is the canonical home for [`ThinkingMode`] because whether and *how* a
//! model reasons is a **model capability**, not a transport detail — exactly
//! the same reason [`crate::effort::Effort`] lives here. The two are
//! **orthogonal**:
//!
//! | concept | meaning | wire surface (Anthropic) |
//! |---------|---------|--------------------------|
//! | `ThinkingMode` | reasoning **on/off** (the switch) | `thinking: {type:"adaptive"}` or omit |
//! | `Effort` | reasoning **depth** (the throttle) | `output_config: {effort: "high"}` |
//!
//! On the Anthropic Messages API these are genuinely independent: a request
//! may set `effort` without enabling thinking (depth has no effect when
//! thinking is off), or enable thinking at any depth. Neenee must therefore
//! model and surface them as two independent knobs — coupling them (e.g.
//! "setting effort also turns thinking on") is a latent bug that prevents the
//! user from expressing legitimate intent such as "high effort, but don't
//! think".
//!
//! `Off` is the default: it omits the `thinking` field. On Opus 4.7/4.8 this
//! disables thinking; on Fable/Mythos thinking is always on and `Off` is a
//! no-op. `Adaptive` emits `thinking: {type:"adaptive"}` (the only on-mode
//! every current model that still accepts a `thinking` object honors; the
//! legacy `enabled`+`budget_tokens` form is rejected with 400 on
//! Fable/Opus-4.7+). Protocol layers (the Anthropic Messages provider) stamp
//! the chosen mode into the wire field; the choice can also ride on a channel
//! as a user override.

/// Whether extended thinking is requested, and which on-mode.
///
/// See the [module docs](self) for the orthogonality with [`crate::effort::Effort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingMode {
    /// Omit the `thinking` field. On Opus 4.7/4.8 this disables thinking; on
    /// Fable/Mythos it is a no-op (thinking is always on there).
    #[default]
    Off,
    /// `thinking: {type: "adaptive"}` — the model decides per request whether
    /// and how much to think. The recommended on-mode for every current model
    /// that still accepts a `thinking` object.
    Adaptive,
}

impl ThinkingMode {
    /// `true` when this mode requests thinking (i.e. emits a `thinking` field
    /// on the wire).
    pub const fn is_on(self) -> bool {
        matches!(self, ThinkingMode::Adaptive)
    }
}
