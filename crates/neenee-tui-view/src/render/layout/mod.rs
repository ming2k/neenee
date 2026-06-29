//! Pluggable transcript layout strategies.
//!
//! `draw_transcript` owns the *frame* — background, viewport carving, footer
//! chrome, sticky pinning — but the actual *arrangement* of messages is
//! delegated here. Each strategy implements [`TranscriptLayout`] and receives a
//! mutable [`Stream`] carrying every piece of shared render state.
//!
//! # The `Stream` contract
//! A layout walks `messages` in order and, for each message, calls the shared
//! helpers on `Stream`:
//!   - [`Stream::badge`]   — the model attribution badge above an assistant turn;
//!   - [`Stream::dispatch`] — the per-kind drawer (notice / tool step / reasoning
//!     trace / message body), including the height-cache fast path;
//!   - [`Stream::gap`]     — insert `n` blank rows of inter-message spacing.
//!
//! These three helpers are the *only* sanctioned mutations of `current_y` /
//! `skip_rows` / `content_lines`, so every layout agrees on scroll accounting
//! and height-cache semantics. A layout is free to add its own chrome (round
//! headers, background bands, …) via the raw paint primitives, but the message
//! body itself always flows through `dispatch`.
//!
//! # Strategies
//! - [`compact::Compact`] — the original flush-stack behavior, preserved
//!   verbatim. The default.
//! - [`round_band::RoundBand`] — option C: each tool round is grouped under a
//!   labelled header (`◆ round N · model · K calls`).
//!
//! New strategies are added by implementing the trait and wiring a match arm
//! in [`Strategy::build`].

pub mod compact;
pub mod round_band;

use neenee_tui::{Frame, Rect};

use crate::document::TranscriptMessage;
use crate::layout::{InteractiveTarget, LayoutMap};
use crate::selection::{CellDragInfo, SelectionState};

use super::HeightCache;
use super::disclosure::StickyStep;
use super::theme::Theme;
use crate::render::design::MESSAGE_GAP_ROWS;

/// Which layout strategy to use for the transcript message stream.
///
/// Selectable via `[tui] transcript_layout` in `config.toml`; the default is
/// [`Strategy::Compact`], which reproduces the original renderer exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    #[default]
    Compact,
    RoundBand,
}

impl Strategy {
    /// Parse a `config.toml` value into a strategy, case-insensitively.
    /// Unknown / empty values fall back to the default (Compact) rather than
    /// erroring, so a typo never blocks startup.
    pub fn from_config(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "round_band" | "roundband" | "round" | "bands" | "grouped" => Self::RoundBand,
            "compact" | "flush" | "default" | "" => Self::Compact,
            _ => Self::Compact,
        }
    }

    /// Construct the concrete layout for this strategy.
    pub fn build(self) -> Box<dyn TranscriptLayout> {
        match self {
            Self::Compact => Box::new(compact::Compact),
            Self::RoundBand => Box::new(round_band::RoundBand),
        }
    }
}

/// The shared render context handed to a layout. Owns the mutable scroll/Y
/// state and the references a layout needs to paint.
///
/// Field visibility is `(pub)` to layouts in this module. `draw_transcript`
/// constructs this once and hands it to `layout.run(&mut stream)`; layouts do
/// not construct it themselves.
///
/// Two lifetime parameters keep variance sane: `'a` is the borrow lifetime of
/// every shared reference (`messages`, `theme`, `layout_map`, …); `'f` is the
/// independent lifetime of the `Frame`'s internal buffer. `Frame` is invariant
/// over its parameter, so unifying `'a` with the frame's lifetime would infect
/// every other field with invariance and trap short-lived locals (like the
/// fallback height cache) in `draw_transcript`.
pub struct Stream<'a, 'f> {
    pub frame: &'a mut Frame<'f>,
    /// The already-inset transcript band every message body renders into.
    pub band: Rect,
    pub messages: &'a [TranscriptMessage],
    pub theme: &'a Theme,
    pub layout_map: &'a mut LayoutMap,
    pub height_cache: &'a mut HeightCache,
    pub selection: &'a SelectionState,
    pub cell_selection: Option<&'a CellDragInfo>,
    pub hovered_step: Option<usize>,
    pub focused_target: Option<InteractiveTarget>,

    // ── mutable scroll / Y accounting ──────────────────────────────────────
    pub current_y: u16,
    pub skip_rows: usize,
    /// Total stream height (un-clipped by the viewport).
    pub content_lines: usize,

    // ── accumulators consumed by `draw_transcript` after the layout returns ─
    pub sticky_steps: Vec<StickyStep>,
}

impl<'a, 'f> Stream<'a, 'f> {
    /// No-op. The per-turn model attribution badge (`provider · model`) was
    /// removed — the round-band header already labels the producing model and
    /// the compact layout needs no per-turn heading. Layouts still call this
    /// unconditionally at the top of each message; keeping the call site means
    /// a future per-turn label can be reintroduced in one place.
    pub fn badge(&mut self, _mi: usize) {}

    /// Dispatch a single message to its per-kind drawer, honoring the
    /// height-cache fast path for skippable (plain-text / notice) messages.
    /// `content_lines` is advanced by the message's true height; `current_y`
    /// stops advancing once it reaches the viewport bottom.
    pub fn dispatch(&mut self, mi: usize) {
        let msg = &self.messages[mi];
        let viewport_bottom = self.band.y + self.band.height;

        let body_before = self.content_lines;
        let skippable =
            msg.is_notice() || (!msg.is_envoy_task() && !msg.is_tool_step() && !msg.is_thinking());
        let cached_height = if skippable {
            self.height_cache.get(msg.id)
        } else {
            None
        };
        let fully_above = cached_height.is_some_and(|h| (h as usize) <= self.skip_rows);
        let fully_below = self.current_y >= viewport_bottom;

        if let Some(h) = cached_height.filter(|_| fully_above || fully_below) {
            // Reproduce exactly the counter mutations a fully-clipped body draw
            // would make, minus the wrapping work.
            self.content_lines += h as usize;
            if fully_above {
                self.skip_rows -= h as usize;
            }
        } else if msg.is_notice() {
            super::draw_notice(
                self.frame,
                self.band,
                msg,
                &mut self.skip_rows,
                &mut self.current_y,
                &mut self.content_lines,
                self.theme,
            );
        } else if msg.is_envoy_task() {
            super::disclosure::draw_envoy_inline_step(
                self.frame,
                self.band,
                msg,
                mi,
                self.theme,
                self.layout_map,
                &mut self.skip_rows,
                &mut self.current_y,
                &mut self.content_lines,
                self.hovered_step == Some(mi),
                self.focused_target == Some(InteractiveTarget::tool_step(mi)),
            );
        } else if msg.is_tool_step() {
            super::disclosure::draw_tool_step(
                self.frame,
                self.band,
                msg,
                mi,
                self.selection,
                self.cell_selection,
                self.theme,
                self.layout_map,
                &mut self.skip_rows,
                &mut self.current_y,
                &mut self.content_lines,
                &mut self.sticky_steps,
                self.hovered_step == Some(mi),
                self.focused_target == Some(InteractiveTarget::tool_step(mi)),
            );
        } else if msg.is_thinking() {
            super::disclosure::draw_reasoning_trace(
                self.frame,
                self.band,
                msg,
                mi,
                self.selection,
                self.cell_selection,
                self.theme,
                self.layout_map,
                &mut self.skip_rows,
                &mut self.current_y,
                &mut self.content_lines,
                &mut self.sticky_steps,
                self.hovered_step == Some(mi),
                self.focused_target == Some(InteractiveTarget::thinking(mi)),
            );
        } else {
            super::draw_message_body(
                self.frame,
                self.band,
                msg,
                mi,
                self.selection,
                self.cell_selection,
                self.theme,
                self.layout_map,
                &mut self.skip_rows,
                &mut self.current_y,
                &mut self.content_lines,
                true,
            );
        }

        // Cache the freshly-measured height for skippable kinds only.
        if skippable && cached_height.is_none() {
            self.height_cache
                .set(msg.id, (self.content_lines - body_before) as u16);
        }
    }

    /// Insert `n` blank rows of inter-message spacing. Consumes `skip_rows`
    /// while still above the viewport, and stops advancing `current_y` at the
    /// viewport bottom. `content_lines` always counts the full height.
    pub fn gap(&mut self, n: usize) {
        self.content_lines += n;
        if self.skip_rows > 0 {
            self.skip_rows = self.skip_rows.saturating_sub(n);
        } else if self.current_y < self.band.y + self.band.height {
            self.current_y = self.current_y.saturating_add(n as u16);
        }
    }

    /// Convenience: one standard inter-message blank row (`MESSAGE_GAP_ROWS`).
    pub fn message_gap(&mut self) {
        self.gap(MESSAGE_GAP_ROWS);
    }

    /// The viewport's bottom y (exclusive). Layouts use it to decide whether a
    /// chrome row (round header) is on-screen before painting it.
    pub fn viewport_bottom(&self) -> u16 {
        self.band.y + self.band.height
    }
}

/// A transcript layout strategy. Implementations walk `messages` via the
/// [`Stream`] helpers and return, leaving `content_lines` / `sticky_steps` /
/// `last_shown_attribution` populated for `draw_transcript`'s post-processing.
pub trait TranscriptLayout {
    fn run(&mut self, stream: &mut Stream<'_, '_>);
}
