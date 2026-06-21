//! Step state machine and presentation primitives.
//!
//! A "step" is any collapsible block in the transcript — a tool step, a
//! sub-agent task, or a reasoning trace. Historically each kind computed its
//! summary-line color from a tangle of ad-hoc flags (`expanded`, `focused`,
//! `hovered`, status…) scattered across the data, interaction, and render
//! layers. That conflation was the root cause of bugs like "a collapsed step
//! stays highlighted because it still carries keyboard focus".
//!
//! This module models a step's state as **three orthogonal axes**, each with
//! a single reason to change, and reduces the visible presentation to pure
//! functions of them. Renderers feed in the axes; this module owns the
//! mapping to color. The axes are:
//!
//! 1. **Lifecycle** — the underlying operation's run state (Running /
//!    Completed / Failed / Denied / Cancelled). Drives the semantic *accent*
//!    (hue). This axis is **kind-specific** and therefore not unified here:
//!    tool steps carry it via [`crate::tui::render::tools::ToolStatus`] (5 states),
//!    reasoning traces via a simple running-bool (2 states). The renderer
//!    resolves it to an accent color and passes that in. See
//!    [`summary_text_color`].
//!
//! 2. **Disclosure** — whether the step's body is shown ([`Disclosure`]).
//!    User-controlled, persisted on the message. Shared by every kind.
//!
//! 3. **Interaction** — transient per-frame pointer/keyboard state
//!    ([`Interaction`]). Recomputed from input each draw, never persisted.
//!    Shared by every kind.
//!
//! The presentation contract is two **independent channels**:
//!
//! - **accent** (hue) — from Lifecycle. A non-completed lifecycle stays
//!   visibly accented even when the step is collapsed and idle, because a
//!   failure/denial must never hide. `Completed` (and reasoning, whose
//!   lifecycle only affects its marker) yield no accent, handing control to
//!   the weight channel.
//! - **weight** (luminance) — from Disclosure × Interaction, via
//!   [`state::summary_weight`]. Decides how bright the summary reads based on
//!   whether it is open or under the pointer — never which color.
//!
//! Keeping the channels separate is what makes the behavior consistent across
//! step kinds and immune to the old "focus leaks into color" class of bug.

use super::Theme;

mod renderers;
mod state;
pub use renderers::{
    draw_reasoning_trace, draw_sticky_summary_if_needed, draw_subagent_bar,
    draw_subagent_inline_step, draw_tool_step, StickyStep,
};
pub use state::{summary_text_color, Disclosure, Interaction};
