//! Built-in tools (filesystem, shell, web, ask-user).
//!
//! Each tool lives in its own module and self-registers via
//! [`neenee_core::register_tool!`] (collected by `inventory` at link time).
//! The binary assembles concrete instances from the registry at startup;
//! this crate does not enumerate them here. Shared helpers live in
//! [`helpers`], and pluggable web-search backends in [`search`].

pub mod commands;
pub mod mcp;
pub mod project;
pub mod search;

mod abort;
mod ask_user;
mod bash;
mod edit;
mod glob;
mod grep;
mod helpers;
mod list;
mod read;
mod read_image;
mod web;
mod write;

// Re-export every tool struct at the crate root so existing consumers
// (`neenee_tools::ReadFileTool`, etc.) keep resolving unchanged.
pub use abort::AbortTool;
pub use ask_user::AskUserTool;
pub use bash::BashTool;
pub use edit::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListDirTool;
pub use project::{CreateProjectTool, InitConfigTool};
pub use read::ReadFileTool;
pub use read_image::ReadImageTool;
pub(crate) use web::html_to_text;
pub use web::{WebFetchTool, WebSearchTool};
pub use write::WriteFileTool;

#[cfg(test)]
mod tests;
