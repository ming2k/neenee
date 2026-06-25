//! Pure domain vocabulary for the coding-agent stack: the `Provider` and
//! `Tool` capability traits, conversation and tool-output types, the
//! context-pressure model, pursuit/repeat/todo domain types, subagent
//! profiles, skills/MCP config schemas, and the wire events the harness and
//! frontends exchange.
//!
//! This crate is **pure domain, zero I/O** (ADR-0005): no `rusqlite`, no
//! filesystem, no network. Persistence-backed types that once lived here
//! (`RepeatStore`, the SQLite migrations) moved to `neenee-store`; this
//! crate keeps only the domain shapes
//! (`Pursuit`, `ThreadPursuit`, `RepeatJob`, `TodoList`, …) and the traits
//! (`Provider`, `Tool`, `Hook`, `SessionReview`, `ContextReliefGate`) the
//! rest of the stack is built on. Pursuit persistence moved onto
//! `SessionStore` (`SessionData.pursuit`) in ADR-0032.

pub use async_trait::async_trait;

pub mod cron;
pub use cron::CronExpr;
pub mod pursuits;
pub mod repeat;
pub use pursuits::{Pursuit, ThreadPursuit, TokenUsage, TurnOutcome, TurnTimer};
pub use repeat::{RepeatJob, DEFAULT_MAX_AGE_DAYS};

pub const PURSUIT_COMPLETE_MARKER: &str = "[NEENEE_PURSUIT_COMPLETE]";

pub mod error;
pub use error::{
    is_context_overflow, parse_retryable_error, public_error_message, retryable_error,
    HarnessError, RetryableError,
};

pub mod message;
pub use message::{
    ImagePart, InjectionKind, InjectionOrigin, Message, Role, ToolCall, ToolResult,
};

pub mod tool_output;
pub use tool_output::{PatchOp, ToolOutput, ToolStream};

pub mod capability;
pub mod catalog;
pub mod events;
pub mod hooks;
pub mod mcp;
pub mod model;
pub mod todos;
pub use todos::{TodoId, TodoItem, TodoList, TodoStatus, TodoToolContext, MAX_TODOS};
pub mod pressure;
pub mod session_review;
pub mod session_title;
pub mod skillsconfig;
pub mod subagent;
pub mod tool_call;
pub mod tool_registry;
pub mod webconfig;
pub use capability::{
    ContextReliefGate, Provider, ProviderStreamEvent, Tool, ToolAccess, WriteScope,
};
pub use catalog::{Channel, ProviderEntry, Transport};
pub use events::{
    AgentEvent, AgentOp, AgentRequest, AgentResponse, HarnessSnapshot, McpServerInfo, ModelInfo,
    ParentStatus, PermissionDecision, PermissionRequest, PermissionRuleInfo, ProviderPickerRow,
    ProviderPickerSnapshot, SessionContextSnapshot, SessionOverview, SkillInfo, SubagentEvent,
    ToolInfo, TurnEvent, UserQuestion, UserQuestionOption, UserQuestionReply, UserQuestionRequest,
};
pub use hooks::{Hook, HookContext, HookEvent, HookEventKind, HookOutcome, SessionSource};
pub use mcp::{McpConnectionStatus, McpServerConfig};
pub use model::{model_by_id, resolve as resolve_model, Model, KNOWN_MODELS};
pub use pressure::{
    estimate_chars, estimate_tokens, prune_tool_results, CompactionPolicy, ContextBudget,
    PruneOutcome, CHARS_PER_TOKEN, CLEARED_TOOL_PREFIX, PRUNED_TOOL_PLACEHOLDER,
};
pub use session_review::{ReviewStatus, ReviewVerdict, SessionReview, DEFAULT_REVIEWER_HARD_STOP};
pub use session_title::{clean_title, TITLE_MAX_LEN};
pub use skillsconfig::SkillsConfig;
pub use subagent::{SubagentProfile, ToolPolicy, EXPLORE, INTERACTIVE, REVIEW, TITLE};
pub use tool_output::truncate_utf8;
pub use tool_registry::{collect_tools, ToolContext, ToolContextBuilder, ToolFactory};
pub use webconfig::WebSearchConfig;
