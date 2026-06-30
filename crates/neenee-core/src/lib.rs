//! Pure domain vocabulary for the neenee agent stack: the `Provider` and
//! `Tool` capability traits, conversation and tool-output types, the
//! context-pressure model, pursuit/repeat/todo domain types, envoy
//! profiles, skills/MCP config schemas, and the wire events the harness and
//! frontends exchange.
//!
//! This crate is **pure domain, zero I/O** (ADR-0005): no `rusqlite`, no
//! filesystem, no network. Persistence-backed types that once lived here
//! (`RepeatStore`, the SQLite migrations) moved to `neenee-store`; this
//! crate keeps only the domain shapes
//! (`Pursuit`, `ThreadPursuit`, `RepeatJob`, `TodoList`, …) and the traits
//! (`Provider`, `Tool`, `Hook`, `SessionReview`, `ContextProjectionGate`) the
//! rest of the stack is built on. Pursuit persistence moved onto
//! `SessionStore` (`SessionData.pursuit`) in ADR-0032.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub use async_trait::async_trait;

pub mod cron;
pub use cron::CronExpr;
pub mod pursuits;
pub mod repeat;
pub use pursuits::{Pursuit, ThreadPursuit, TokenUsage, TurnOutcome, TurnTimer};
pub use repeat::{DEFAULT_MAX_AGE_DAYS, RepeatJob};

pub const PURSUIT_COMPLETE_MARKER: &str = "[NEENEE_PURSUIT_COMPLETE]";

pub mod error;
pub use error::{
    HarnessError, RetryableError, is_context_overflow, parse_retryable_error, public_error_message,
    retryable_error,
};

pub mod message;
pub use message::{ImagePart, InjectionKind, InjectionOrigin, Message, Role, ToolCall, ToolResult};

pub mod tool_output;
pub use tool_output::{
    PatchOp, ShellTermination, StdinPolicy, ToolOutput, ToolStream, is_interactive_command,
};

pub mod capability;
pub mod catalog;
pub mod effort;
pub use effort::{EFFORT_CLAUDE_FULL, EFFORT_CLAUDE_NO_XHIGH, EFFORT_COMMON, Effort};
pub mod thinking;
pub use thinking::{ThinkingMode, ThinkingSupport};
pub mod dynamic;
pub mod events;
pub mod hooks;
pub mod mcp;
pub mod model;
pub mod todos;
pub use todos::{MAX_TODOS, TodoId, TodoItem, TodoList, TodoStatus, TodoToolContext};
pub mod envoy;
pub mod pressure;
pub mod token_ledger;
pub use token_ledger::{TokenSourceLedger, TokenSourceReport, TokenSourceRow, TokenSourceTotals};
pub mod nudgeconfig;
pub mod prompt;
pub mod session_review;
pub mod session_title;
pub mod skillsconfig;
pub mod tool_call;
pub mod tool_registry;
pub mod webconfig;
pub use capability::{
    CommandScope, ContextProjectionGate, OperationScope, Provider, ProviderStreamEvent,
    ScopeTarget, Tool, VariantSelection, empty_variant_selection,
};
pub use catalog::{Channel, ProviderEntry, Transport};
pub use dynamic::DynamicCatalog;
pub use envoy::{EXPLORE, EnvoyProfile, INTERACTIVE, QUANT, REVIEW, TITLE, ToolPolicy};
pub use events::{
    AgentEvent, AgentNotice, AgentOp, AgentRequest, AgentResponse, EnvoyEvent, HarnessSnapshot,
    InputReply, InputRequest, McpServerInfo, ModelInfo, NoticeKind, NoticeSeverity, NoticeSource,
    NoticeSurface, ParentStatus, PermissionDecision, PermissionRequest, PermissionRuleInfo,
    ProviderModelInfo, ProviderPickerRow, ProviderPickerSnapshot, SessionContextSnapshot,
    SessionOverview, SkillInfo, ToolInfo, TurnEvent, UserQuestion, UserQuestionOption,
    UserQuestionReply, UserQuestionRequest,
};
pub use hooks::{Hook, HookContext, HookEvent, HookEventKind, HookOutcome, SessionSource};
pub use mcp::{McpConnectionStatus, McpServerConfig};
pub use model::{KNOWN_MODELS, Model, WireFormat, model_by_id, resolve as resolve_model};
pub use nudgeconfig::NudgeConfig;
pub use pressure::{
    CHARS_PER_TOKEN, CLEARED_TOOL_PREFIX, CompactionPolicy, ContextBudget, PRUNED_TOOL_PLACEHOLDER,
    PruneOutcome, count_tokens, estimate_bytes, estimate_tokens, prune_tool_results,
};
pub use prompt::{PromptChannel, PromptContext, PromptRegistry, PromptSection};
pub use session_review::{DEFAULT_REVIEWER_HARD_STOP, ReviewStatus, ReviewVerdict, SessionReview};
pub use session_title::{TITLE_MAX_LEN, clean_title};
pub use skillsconfig::SkillsConfig;
pub use tool_output::truncate_utf8;
pub use tool_registry::{
    Capability, ToolContext, ToolContextBuilder, ToolFactory, ToolScope, ToolSelection, ToolSet,
    collect_toolset,
};
pub use webconfig::WebSearchConfig;
