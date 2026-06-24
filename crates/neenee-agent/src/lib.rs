//! The orchestration layer between the pure domain (`neenee-core`) and the
//! application services (`neenee-store`) on one side, and the frontends on the
//! other.
//!
//! # What lives here
//!
//! - **The `Agent` struct** (`agent.rs`) — holds the provider, tool set, mode,
//!   pursuit, and skill registry; runs the streaming ReAct loop
//!   (`run_streaming_with_events`).
//! - **System-prompt assembly** (`prompt.rs`) — methods extending `Agent` that
//!   rebuild the system message each turn and auto-load mentioned skills.
//! - **Skill registry + discovery** (`skills/`) — loads skills from disk and
//!   remote indices; produces the `UseSkillTool` / `ListSkillsTool` /
//!   `ReloadSkillsTool` tool implementations.
//! - **Turn orchestration** (`orchestration.rs`) — the policy that wraps every
//!   agent turn: compaction, mid-turn pruning, retries with backoff, the
//!   `/pursue` stop-gate driver, and the `/repeat` cron scheduler. Frontends drive the harness
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
    AgentMode, AgentRequest, AgentResponse, Channel, ContextReliefGate, HarnessError,
    HarnessSnapshot, ImagePart, McpConnectionStatus, McpServerConfig, Message, PatchOp,
    PermissionDecision, PermissionRequest, Provider, ProviderEntry, ProviderPickerRow,
    ProviderPickerSnapshot, ProviderStreamEvent, PruneOutcome, Pursuit, PursuitService,
    PursuitStore, RetryableError, Role, SessionOverview, SkillsConfig, SubTaskEvent,
    SubagentProfile, TokenUsage, Tool, ToolAccess, ToolCall, ToolOutput, ToolPolicy, ToolResult,
    ToolStream, Transport, TurnOutcome, TurnTimer, UserQuestion, UserQuestionOption,
    UserQuestionReply, UserQuestionRequest, WebSearchConfig, EXPLORE, PRUNED_TOOL_PLACEHOLDER,
    TITLE, VERIFY,
};

// Same ambient std/tokio prelude the Agent struct used to inherit from
// `neenee-core`'s lib.rs (`use super::*`).
use futures::{future::join_all, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Maximum number of times the same tool call (same name + same arguments)
/// may repeat within a turn before the agent treats it as a stuck loop and
/// errors out. This is the only per-turn backstop: distinct tool calls are
/// allowed to run unbounded, matching the codex / claude-code model where
/// the agentic loop runs until the model itself stops calling tools. Context
/// compaction (thresholds derived from the active model's context window, plus
/// mid-turn pruning) keeps the transcript bounded; the user can interrupt at
/// any time with `Esc` or `/pursue stop`.
const MAX_REPEATED_TOOL_CALLS: usize = 3;

/// Safety cap on the number of rounds the `/pursue` stop-gate will drive
/// within a single turn. Prevents a pursuit that never signals completion
/// from looping forever; the user can also interrupt with `Esc`. Generous by
/// design — a well-behaved pursuit completes by signalling the marker well
/// before this.
///
/// This is **not** the per-turn round cap ADR-0009 removed: an ordinary turn
/// (no pursuit armed) stays uncapped and ends when the model stops calling
/// tools. This cap only bounds the *forced re-injection* of an opt-in stop-gate
/// the user explicitly armed — see ADR-0015.
const MAX_PURSUIT_ITERATIONS: u32 = 50;

/// Maximum interval between consecutive stream events (text/reasoning/tool-call
/// deltas) before the stream is considered stalled. All LLM providers use
/// `reqwest::Client::new()` which sets no read timeout, so without this guard a
/// reasoning model whose SSE connection hangs mid-generation (server stops
/// sending but keeps the TCP connection alive) blocks the turn loop
/// indefinitely — the UI spins "running · responding" forever and only a user
/// interrupt can break it. The bound is generous: reasoning models stream
/// deltas frequently and SSE keepalives arrive every 15–30 s, so two full
/// minutes of total silence is a genuine stall. On timeout the harness
/// surfaces a retryable error so the turn retries with backoff instead of
/// hanging.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Overall timeout for a single non-streaming `provider.chat()` call. The
/// non-streaming ReAct path ([`Agent::run_with_events`]) and context-
/// compaction summarization both call `chat()`, which blocks until the model
/// returns the complete response. Without a bound, a stalled or overloaded
/// endpoint hangs the turn (and, for compaction, the entire frontend) forever.
/// Five minutes is generous enough for a reasoning model generating a full
/// non-streaming response, while still catching a genuine stall. On timeout
/// the caller surfaces a retryable / fallback error instead of hanging.
const CHAT_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub mod agent;
pub use agent::Agent;

pub mod catalog;
pub mod hooks;
pub use hooks::{matcher_matches, HookRegistry, UserPromptVerdict};
pub mod orchestration;
mod plan_verify;
mod prompt;
pub mod session_review;
pub mod session_title;
pub mod skills;
pub mod task_tool;

pub use plan_verify::VerifyPlanExecutionTool;
pub use session_review::{default_reviews, LoopingReview};
pub use task_tool::TaskTool;

#[cfg(test)]
mod tests;
