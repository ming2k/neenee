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
//! - [`config`] — config manager modal (root settings overlay)
//! - [`config_nudge`] — nudge sub-page of the config manager
//! - [`config_layout`] — transcript layout sub-page of the config manager
//! - [`toast`] — copy / armed-action notice bubbles
//! - [`common`] — shared helpers (time formatting, truncation, caret, glyphs)

pub(crate) mod activity;
pub(crate) mod common;
pub(crate) mod config;
pub(crate) mod config_layout;
pub(crate) mod config_nudge;
pub(crate) mod help;
pub(crate) mod history;
pub(crate) mod mcp;
pub(crate) mod permission;
pub(crate) mod permissions_manager;
pub(crate) mod provider;
pub(crate) mod session;
pub(crate) mod toast;
pub(crate) mod token_report;
pub(crate) mod tools;

// Re-export the public API so `render::overlays::draw_*` callers are unchanged.
pub use activity::{ActivityModalView, draw_activity_modal};
pub(crate) use config::draw_config_modal;
pub(crate) use config_layout::draw_config_layout_modal;
pub(crate) use config_nudge::draw_config_nudge_modal;
pub use help::draw_help_modal;
pub use history::draw_history_modal;
pub(crate) use mcp::draw_mcp_modal;
pub use permission::{draw_input_injection, draw_permission_sheet, draw_question_modal};
pub(crate) use permissions_manager::draw_permissions_manager;
pub(crate) use provider::{
    CustomEditorView, draw_add_model_editor, draw_custom_provider_editor, draw_model_editor,
    draw_models_modal, draw_provider_template_chooser,
};
pub use session::{draw_session_modal, draw_sessions_modal};
pub use toast::{draw_armed_toast, draw_copy_toast};
pub(crate) use token_report::draw_token_report_modal;
pub(crate) use tools::draw_tools_modal;
