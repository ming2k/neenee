//! Overlay modal renderers, split by functional domain.
//!
//! Sub-modules:
//! - [`provider`] — provider picker + API-key / model-id editor
//! - [`session`] — sessions picker + session-context dashboard modal
//! - [`tools`] — tools manager modal (the interactive tool-list surface)
//! - [`mcp`] — MCP manager modal (per-server enable/reconnect surface)
//! - [`activity`] — activity modal (pursuit, prompt, status, or todos)
//! - [`permission`] — permission sheet + question modal
//! - [`history`] — history search modal
//! - [`help`] — help / keybindings modal
//! - [`toast`] — copy / armed-action notice bubbles
//! - [`common`] — shared helpers (time formatting, truncation, caret, glyphs)

pub(crate) mod activity;
pub(crate) mod common;
pub(crate) mod help;
pub(crate) mod history;
pub(crate) mod mcp;
pub(crate) mod permission;
pub(crate) mod permissions_manager;
pub(crate) mod provider;
pub(crate) mod session;
pub(crate) mod toast;
pub(crate) mod tools;

// Re-export the public API so `render::overlays::draw_*` callers are unchanged.
pub use activity::{ActivityModalView, draw_activity_modal};
pub use help::draw_help_modal;
pub use history::draw_history_modal;
pub(crate) use mcp::draw_mcp_modal;
pub use permission::{draw_permission_sheet, draw_question_modal};
pub(crate) use permissions_manager::draw_permissions_manager;
pub(crate) use provider::{draw_model_editor, draw_models_modal};
pub use session::{draw_session_modal, draw_sessions_modal};
pub use toast::{draw_armed_toast, draw_copy_toast};
pub(crate) use tools::draw_tools_modal;
