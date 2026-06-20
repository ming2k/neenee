//! The orchestration layer between the pure domain (`neenee-core`) and the
//! application services (`neenee-store`) on one side, and the frontends on the
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
//! `neenee-agent` is the wiring layer: it depends on `neenee-core`
//! (domain vocabulary), `neenee-store` (durable state: `SessionStore`,
//! `Config`, `EmbeddingStore`), and `neenee-providers` (the
//! `build_provider_for_channel` factory plus the user-agent / spec
//! constants the catalog uses when constructing concrete impls). It
//! speaks to tools through the core `Tool` trait, so it does **not**
//! depend on `neenee-tools` at lib build time — concrete tool instances
//! are constructed by the binary, which depends on everything.
//! (`neenee-tools` is a *dev*-dependency so the
//! `ask_user_tool_blocks_and_returns_selected_answers` integration test
//! can construct a real `AskUserTool`; dev-deps do not form cycles.)
//!
//! ## Why catalog and TaskTool live here (not in store / tools)
//!
//! Both got relocated here from their intuitive homes to keep the
//! dependency graph strictly layered (see ADR-0005):
//!
//! - **`catalog`** builds concrete `Provider` impls from a `Config`. It
//!   used to live in `neenee-store`, which forced store to depend on
//!   `neenee-providers` — an inversion, since store is otherwise a peer
//!   of providers. The catalog is fundamentally a factory consumed by
//!   orchestration, so it lives where orchestration lives.
//! - **`TaskTool`** spawns sub-agents via `Agent::new`. It used to live
//!   in `neenee-tools`, which forced tools to depend on this crate —
//!   another inversion, since tools are below the agent layer. The
//!   task tool is fundamentally an orchestration primitive that
//!   happens to satisfy the `Tool` trait, so it lives here too.
//!
//! Everything `neenee-core` exports is re-exported here so consumers can
//! `use neenee_agent::*` and get the full domain vocabulary alongside the
//! orchestration API.

pub use neenee_core::*;

// Explicit re-exports of core's top-level re-exports. `pub use X::*` does
// not propagate through X's own `pub use` re-exports in Rust, so the items
// the Agent struct expects at the crate root have to be listed here by name.
// Keep this list in sync with `neenee_core`'s lib.rs re-exports.
pub use neenee_core::{
    estimate_chars, estimate_tokens, is_context_overflow, parse_retryable_error,
    prune_tool_results, public_error_message, retryable_error, truncate_utf8, AgentEvent,
    AgentMode, AgentRequest, AgentResponse, Channel, CompactionGate, Goal,
    GoalAccountingResult, GoalChecklistItem, GoalChecklistStatus, GoalService, GoalStatus,
    GoalStore, HarnessError, HarnessSnapshot, ImagePart, McpConnectionStatus, McpServerConfig,
    Message, ModelEntry, ModelPickerRow, ModelPickerSnapshot, PatchOp, PermissionDecision,
    PermissionRequest, Provider, ProviderStreamEvent, PruneOutcome, RetryableError, Role,
    SessionOverview, SkillsConfig, SubTaskEvent, TokenUsage, Tool, ToolAccess, ToolCall,
    ToolOutput, ToolResult, ToolStream, Transport, TurnOutcome, TurnTimer, UserQuestion,
    UserQuestionOption, UserQuestionReply, UserQuestionRequest, WebSearchConfig,
    PRUNED_TOOL_PLACEHOLDER,
};

// Same ambient std/tokio prelude the Agent struct used to inherit from
// `neenee-core`'s lib.rs (`use super::*`).
use futures::{future::join_all, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
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
