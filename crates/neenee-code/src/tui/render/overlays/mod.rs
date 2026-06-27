//! Overlay modal renderers, split by functional domain.
//!
//! Sub-modules:
//! - [`provider`] — provider picker + API-key / model-id editor
//! - [`session`] — sessions picker + session-context dashboard modal
//! - [`activity`] — activity modal (pursuit, prompt, status, or todos)
//! - [`permission`] — permission sheet + question modal
//! - [`history`] — history search modal
//! - [`help`] — help / keybindings modal
//! - [`tool_step_detail`] — full-output detail overlay for a focused tool step
//! - [`toast`] — copy / armed-action notification bubbles
//! - [`common`] — shared helpers (time formatting, truncation, caret, glyphs)

pub(crate) mod activity;
pub(crate) mod common;
pub(crate) mod config;
pub(crate) mod help;
pub(crate) mod history;
pub(crate) mod permission;
pub(crate) mod permissions_manager;
pub(crate) mod provider;
pub(crate) mod session;
pub(crate) mod toast;
pub(crate) mod tool_step_detail;

// Re-export the public API so `render::overlays::draw_*` callers are unchanged.
pub use activity::{ActivityModalView, draw_activity_modal};
pub use config::draw_config_modal;
pub use help::draw_help_modal;
pub use history::draw_history_modal;
pub use permission::{draw_permission_sheet, draw_question_modal};
pub(crate) use permissions_manager::draw_permissions_manager;
pub(crate) use provider::{draw_model_editor, draw_model_picker, draw_models_modal};
pub use session::{draw_session_modal, draw_sessions_modal};
pub use toast::{draw_armed_toast, draw_copy_toast};
pub use tool_step_detail::draw_tool_step_detail_overlay;
