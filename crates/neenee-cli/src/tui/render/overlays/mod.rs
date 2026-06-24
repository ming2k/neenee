//! Overlay modal renderers, split by functional domain.
//!
//! Sub-modules:
//! - [`provider`] — provider picker + API-key / model-id editor
//! - [`session`] — sessions picker + session-context tabbed modal
//! - [`activity`] — activity modal (pursuit, prompt, tasks)
//! - [`permission`] — permission sheet + question modal
//! - [`misc`] — history search, tool-step detail, help, plan preview, toasts
//! - [`common`] — shared helpers (time formatting, truncation, caret, glyphs)

pub(crate) mod activity;
pub(crate) mod common;
pub(crate) mod misc;
pub(crate) mod permission;
pub(crate) mod provider;
pub(crate) mod session;

// Re-export the public API so `render::overlays::draw_*` callers are unchanged.
pub use activity::{draw_activity_modal, ActivityModalView};
pub use misc::{
    draw_armed_toast, draw_copy_toast, draw_help_modal, draw_history_modal,
    draw_plan_preview_modal, draw_tool_step_detail_overlay,
};
pub use permission::{draw_permission_sheet, draw_question_modal};
pub(crate) use provider::draw_models_modal;
pub use provider::draw_model_editor;
pub use session::{draw_session_modal, draw_sessions_modal};
