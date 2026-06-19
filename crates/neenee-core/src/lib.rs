pub use async_trait::async_trait;
use futures::{future::join_all, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub mod goals;
pub use goals::{
    Goal, GoalAccountingResult, GoalChecklistItem, GoalChecklistStatus, GoalService, GoalStatus,
    GoalStore, TokenUsage, TurnOutcome, TurnTimer,
};

const MAX_TOOL_ROUNDS: usize = 32;
const MAX_REPEATED_TOOL_CALLS: usize = 3;
pub const GOAL_COMPLETE_MARKER: &str = "[NEENEE_GOAL_COMPLETE]";

pub mod error;
pub use error::{
    is_context_overflow, parse_retryable_error, public_error_message, retryable_error,
    HarnessError, RetryableError,
};

pub mod message;
pub use message::{ImagePart, Message, Role, ToolCall, ToolResult};

pub mod tool_output;
pub use tool_output::{PatchOp, ToolOutput, ToolStream};

pub mod capability;
pub mod catalog;
pub mod commands;
pub mod events;
pub mod mcp;
pub mod plan;
pub mod pressure;
pub mod project;
mod prompt;
pub mod providers;
pub mod skills;
mod tool_call;
pub mod tools;

pub use capability::{CompactionGate, Provider, ProviderStreamEvent, Tool, ToolAccess};
pub use catalog::{Catalog, Channel, ModelEntry, Transport};
pub use events::{
    AgentEvent, AgentMode, AgentRequest, AgentResponse, HarnessSnapshot, ModelPickerRow,
    ModelPickerSnapshot, PermissionDecision, PermissionRequest, SessionOverview, SubTaskEvent,
    UserQuestion, UserQuestionOption, UserQuestionReply, UserQuestionRequest,
};
pub use pressure::{
    estimate_chars, estimate_tokens, prune_tool_results, PruneOutcome, PRUNED_TOOL_PLACEHOLDER,
};

mod agent;
pub use agent::Agent;

#[cfg(test)]
mod tests;
