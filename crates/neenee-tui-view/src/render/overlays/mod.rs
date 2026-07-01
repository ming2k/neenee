//! Overlay modal renderers, split by functional domain.
//!
//! Sub-modules:
//! - [`provider`] — provider picker + API-key / model-id editor
//! - [`session`] — sessions picker + session-context dashboard modal
//! - [`tools`] — tools manager modal (the interactive tool-list surface)
//! - [`skills`] — skills modal (loaded-skill list with detail expansion)
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

pub mod activity;
pub mod common;
pub mod config;
pub mod config_layout;
pub mod config_nudge;
pub mod debug;
pub mod help;
pub mod history;
pub mod mcp;
pub mod permission;
pub mod permissions_manager;
pub mod provider;
pub mod session;
pub mod skills;
pub mod toast;
pub mod token_report;
pub mod tools;

// Re-export the public API so `render::overlays::draw_*` callers are unchanged.
pub use activity::{ActivityModalView, draw_activity_modal};
pub use config::draw_config_modal;
pub use config_layout::draw_config_layout_modal;
pub use config_nudge::draw_config_nudge_modal;
pub use debug::{DebugDetail, DebugSection, draw_debug_modal};
pub use help::draw_help_modal;
pub use history::draw_history_modal;
pub use mcp::draw_mcp_modal;
pub use permission::{draw_input_injection, draw_permission_sheet, draw_question_modal};
pub use permissions_manager::draw_permissions_manager;
pub use provider::{
    CustomEditorView, draw_add_model_editor, draw_custom_provider_editor, draw_model_editor,
    draw_models_modal, draw_provider_template_chooser,
};
pub use session::draw_sessions_modal;
pub use skills::draw_skills_modal;
pub use toast::{draw_armed_toast, draw_copy_toast};
pub use token_report::draw_token_report_modal;
pub use tools::draw_tools_modal;
