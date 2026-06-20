//! The orchestration layer between the pure domain (`neenee-core`) and the
//! application services (`neenee-app`) on one side, and the frontends on the
//! other.
//!
//! # What lives here
//!
//! - **The `Agent` struct** (`agent.rs`) — holds the provider, tool set, mode,
//!   goal, and skill registry; runs the streaming ReAct loop
//!   (`run_streaming_with_events`).
//! - **System-prompt assembly** (`prompt.rs`) — methods extending `Agent` that
//!   rebuild the system message each turn and auto-load mentioned skills.
//! - **Skill registry + discovery** (`skills/`) — loads skills from disk and
//!   remote indices; produces the `UseSkillTool` / `ListSkillsTool` /
//!   `ReloadSkillsTool` tool implementations.
//! - **Turn orchestration** (`orchestration.rs`) — the policy that wraps every
//!   agent turn: compaction, mid-turn pruning, retries with backoff, goal
//!   accounting, and the autonomous goal loop. Frontends drive the harness
//!   through [`orchestration::execute_turn`] and friends; they own only the
//!   UI-specific input path (slash commands for the CLI, menus/dialogs for a
//!   future GUI).
//!
//! # Dependency posture
//!
//! `neenee-harness` depends on `neenee-core` (domain vocabulary) and
//! `neenee-app` (durable state: `SessionStore`, `Config`, `EmbeddingStore`).
//! It speaks to providers and tools through the core `Provider` / `Tool`
//! traits, so it does **not** depend on `neenee-providers` or `neenee-tools`
//! at lib build time — concrete provider/tool instances are constructed by
//! the binary, which depends on everything. (`neenee-tools` depends on
//! `neenee-harness` for `Agent` because `TaskTool` spawns sub-agents; the
//! relationship is one-way, no cycle.)
//!
//! Everything `neenee-core` exports is re-exported here so consumers can
//! `use neenee_harness::*` and get the full domain vocabulary alongside the
//! orchestration API.

pub use neenee_core::*;

// Explicit re-exports of core's top-level re-exports. `pub use X::*` does
// not propagate through X's own `pub use` re-exports in Rust, so the items
// the Agent struct expects at the crate root have to be listed here by name.
// Keep this list in sync with `neenee_core`'s lib.rs re-exports.
pub use neenee_core::{
    Goal, GoalAccountingResult, GoalChecklistItem, GoalChecklistStatus, GoalService, GoalStatus,
    GoalStore, TokenUsage, TurnOutcome, TurnTimer, is_context_overflow,
    parse_retryable_error, public_error_message, retryable_error, HarnessError, RetryableError,
    ImagePart, Message, Role, ToolCall, ToolResult, PatchOp, ToolOutput, ToolStream,
    CompactionGate, Provider, ProviderStreamEvent, Tool, ToolAccess, Catalog, Channel, ModelEntry,
    Transport, AgentEvent, AgentMode, AgentRequest, AgentResponse, HarnessSnapshot,
    ModelPickerRow, ModelPickerSnapshot, PermissionDecision, PermissionRequest, SessionOverview,
    SubTaskEvent, UserQuestion, UserQuestionOption, UserQuestionReply, UserQuestionRequest,
    estimate_chars, estimate_tokens, prune_tool_results, PruneOutcome, PRUNED_TOOL_PLACEHOLDER,
    WebSearchConfig, McpConnectionStatus, McpServerConfig, SkillsConfig, truncate_utf8,
};

// Same ambient std/tokio prelude the Agent struct used to inherit from
// `neenee-core`'s lib.rs (`use super::*`).
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use futures::{future::join_all, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Cap on the number of tool rounds within a single turn. Prevents runaway
/// tool loops from burning the entire context budget.
const MAX_TOOL_ROUNDS: usize = 32;

/// Maximum number of times the same tool call (same name + same arguments)
/// may repeat within a turn before the agent treats it as a stuck loop and
/// errors out.
const MAX_REPEATED_TOOL_CALLS: usize = 3;

pub mod agent;
pub use agent::Agent;

pub mod catalog;
pub mod orchestration;
mod prompt;
pub mod skills;
pub mod task_tool;

pub use task_tool::TaskTool;

#[cfg(test)]
mod tests;
