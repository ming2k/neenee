//! neenee TUI **view layer** — the widgets and semantic document model that
//! render the agent transcript and its overlays.
//!
//! This crate sits between the in-house [`neenee_tui`] engine (a retained cell
//! grid with dirty tracking and a back/front diff; ADR-0038) and the app shell
//! (`neenee_code::tui`, which owns `App` state, the event loop, and input
//! mapping). It renders neenee_core domain types — so it depends on
//! [`neenee_core`] — but it never depends on the shell: the seam is the
//! borrowed [`render::TranscriptView`] struct the shell fills in each frame.
//!
//! Layering:
//! ```text
//! neenee-tui (engine: cell grid, diff, crossterm backend)
//!         ▲ render into the grid
//! neenee-tui-view (THIS crate: widgets + document model)   depends on neenee-core
//!         ▲ TranscriptView<'a> seam
//! neenee-code::tui (app shell: App, event loop, input)
//! ```
//!
//! Modules:
//! - [`render`] — the widget tree (transcript, steps, tools, overlays, chrome).
//! - [`document`] — the semantic document model (`TranscriptMessage`, `Block`).
//! - [`layout`] — `LayoutMap`, hit-testing, `SemanticCursor`.
//! - [`selection`] — text/cell selection state.
//! - [`fuzzy`] — fuzzy matcher used by the history / provider overlays.
//! - [`providers`] — provider/model picker ranking + display helpers.
//! - [`modal`] — [`modal::Modal`] / [`modal::Recess`] / [`modal::ActivityTab`]
//!   discriminants shared with the shell.
//! - [`completion`] — completion-menu data types (the matching logic stays in
//!   the shell).

pub mod completion;
pub mod document;
pub mod fuzzy;
pub mod layout;
pub mod modal;
pub mod providers;
pub mod render;
pub mod selection;
