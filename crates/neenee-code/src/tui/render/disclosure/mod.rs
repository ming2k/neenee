//! Disclosure state machine and presentation primitives.
//!
//! A **disclosure** is any collapsible block in the transcript — a tool step, a
//! envoy task, or a reasoning trace — sharing one summary/body model and one
//! color contract. (The leaf renderers keep their kind-specific names —
//! [`draw_tool_step`], [`draw_reasoning_trace`], [`draw_envoy_inline_step`] —
//! since only a tool call really reads as a "step"; this module is the umbrella
//! abstraction over all three.) Historically each kind computed its
//! summary-line color from a tangle of ad-hoc flags (`expanded`, `focused`,
//! `hovered`, status…) scattered across the data, interaction, and render
//! layers. That conflation was the root cause of bugs like "the focused step's
//! text never lights up because the render layer discarded the focus flag".
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
//! The presentation contract is two **composable channels**, joined in
//! [`state::summary_text_color`]:
//!
//! - **accent** (hue) — from Lifecycle. A non-completed lifecycle stays
//!   visibly accented even when the step is collapsed and idle, because a
//!   failure/denial must never hide. `Completed` (and reasoning, whose
//!   lifecycle only affects its marker) yield no accent, handing control to
//!   the weight channel.
//! - **weight** (luminance) — from Disclosure × Interaction, via
//!   [`state::summary_weight`]. A **disclosure-first, monotonic model** decides
//!   how bright the summary reads — interaction may only ever *lift* a summary,
//!   never darken it:
//!   1. Expanded → the primary foreground, **pinned**. An open body is already
//!      the active state, so hover/focus on it is a no-op (the old rule dropped
//!      an expanded summary to the dimmer hover tone, making it recede when you
//!      pointed at it — the bug this model removes).
//!   2. Collapsed + hover/focus → the intermediate hover tone (the "this opens"
//!      affordance). Keyboard focus shares this tone.
//!   3. Collapsed + idle → muted (a closed step recedes).
//!
//!   The three tones stay distinct and ordered (`muted` < `hover` < `fg`):
//!   closing a step immediately darkens it to muted, and the only way to reach
//!   `fg` is to open it.
//!
//! When an accent is present it supplies the hue and the weight channel
//! modulates its brightness (see [`state::summary_text_color`]), so an accent
//! step — e.g. a long-running envoy task — still shifts tone on hover (while
//! collapsed) and leans to the foreground when open, instead of sitting at one
//! flat color. Keeping the channels composable is what makes the behavior
//! consistent across step kinds: a collapsed step lifts toward the hover tone
//! when hovered or focused, an open step pins to the foreground, and each cause
//! flows through the single [`state::summary_weight`] entry point.

use super::Theme;

mod renderers;
mod state;
pub use renderers::{
    StickyStep, draw_envoy_bar, draw_envoy_inline_step, draw_reasoning_trace, draw_side_banner,
    draw_sticky_summary_if_needed, draw_tool_step,
};
pub use state::{Disclosure, Interaction, summary_text_color};
