//! Lifecycle event hooks (ADR-0025): user-configurable interception at
//! session, turn, and tool-call points.
//!
//! neenee keeps a single event axis — the context-threshold, round-count, and
//! clock concerns are already owned by `CompactionPolicy`, `/pursue`, and
//! `/repeat` and are deliberately **not** re-exposed here. The capability a
//! hook has (block / inject / observe) is implicit in the event it fires on,
//! matching Claude Code's model: a `PreToolUse` hook may deny, a `Stop` hook
//! may force another round, the rest only observe or inject context.
//!
//! v1 ships a single command-handler implementation (see `neenee_code`); the
//! [`Hook`] trait lives here so the registry and insertion points in
//! `neenee_agent` stay frontend-agnostic and so future handler types
//! (`http`, `mcp_tool`) slot in without re-touching the loop.

use crate::async_trait;
use serde::{Deserialize, Serialize};

/// How a session started. Surfed as the `SessionStart` source/matcher value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    Startup,
    Resume,
}

/// Which lifecycle point a hook fires on — the routing key only. The payload
/// travels in [`HookContext`]; matcher evaluation lives in the registry
/// (`neenee_agent::hooks`), not here, so core stays free of the `regex` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEventKind {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    PreCompact,
    PostCompact,
    /// Fires once per tool round (ADR-0030). Constrained: only `Inject` is
    /// honoured — `Deny` is ignored so a round-count hook cannot become a
    /// de-facto round cap (the ADR-0009 concern). The harness declares no
    /// built-in threshold on this axis; it only provides the trigger point.
    Round,
}

impl HookEventKind {
    /// Whether this event filters on a tool name and so honours a matcher.
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure
        )
    }
}

/// Owned snapshot of the moment a hook fires. Serialized to JSON and piped to
/// command handlers on stdin. Owned (not borrowed) so it crosses the async
/// spawn into the command runner without lifetime gymnastics.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub session_id: String,
    pub cwd: Option<std::path::PathBuf>,
    pub event: HookEvent,
}

/// The payload for one fire. Tool events carry a name + a reduced view of the
/// input/output — commands read JSON on stdin, not live Rust values, so the
/// full [`crate::ToolOutput`] (which may embed a subagent transcript) is not
/// forwarded wholesale; its `to_text()` summary is.
#[derive(Debug, Clone)]
pub enum HookEvent {
    SessionStart {
        source: SessionSource,
    },
    SessionEnd,
    UserPromptSubmit {
        prompt: String,
    },
    PreToolUse {
        tool_name: String,
        tool_input: serde_json::Value,
    },
    PostToolUse {
        tool_name: String,
        tool_output: String,
        duration_ms: u64,
    },
    PostToolUseFailure {
        tool_name: String,
        error: String,
    },
    Stop {
        last_message: String,
    },
    PreCompact,
    PostCompact,
    /// Fires once per tool round (ADR-0030). `consecutive_readonly` carries the
    /// read-only-round streak so a hook can act on "exploration without
    /// progress" without re-deriving it. Only `Inject` is honoured (see
    /// [`HookEventKind::Round`]).
    Round {
        round: usize,
        consecutive_readonly: u32,
    },
}

impl HookEvent {
    pub fn kind(&self) -> HookEventKind {
        match self {
            Self::SessionStart { .. } => HookEventKind::SessionStart,
            Self::SessionEnd => HookEventKind::SessionEnd,
            Self::UserPromptSubmit { .. } => HookEventKind::UserPromptSubmit,
            Self::PreToolUse { .. } => HookEventKind::PreToolUse,
            Self::PostToolUse { .. } => HookEventKind::PostToolUse,
            Self::PostToolUseFailure { .. } => HookEventKind::PostToolUseFailure,
            Self::Stop { .. } => HookEventKind::Stop,
            Self::PreCompact => HookEventKind::PreCompact,
            Self::PostCompact => HookEventKind::PostCompact,
            Self::Round { .. } => HookEventKind::Round,
        }
    }

    /// Tool name when this is a tool event; `None` otherwise (the matcher is
    /// then ignored).
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::PreToolUse { tool_name, .. }
            | Self::PostToolUse { tool_name, .. }
            | Self::PostToolUseFailure { tool_name, .. } => Some(tool_name),
            _ => None,
        }
    }
}

/// What a hook decided. The effect each variant has depends on the firing
/// event; an irrelevant variant returned by a handler is ignored, so a command
/// that unconditionally prints `{"decision":"deny"}` only bites on events that
/// honour denial.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HookOutcome {
    /// No effect. The default.
    #[default]
    Pass,
    /// `PreToolUse`: the call is blocked; `reason` becomes the tool error the
    /// model sees. `Stop`: the turn continues for another round with `reason`
    /// fed back as a hidden user message. Ignored on other events, including
    /// `Round` (ADR-0030: a round-count hook may not become a de-facto cap).
    Deny { reason: String },
    /// Inject `context` as a hidden user message the model sees on its next
    /// round. Honoured on `UserPromptSubmit` (prepended), `Stop`,
    /// `PostToolUse`, and `Round`. Ignored elsewhere.
    Inject { context: String },
}

/// One user-configurable lifecycle hook (ADR-0025). A hook declares the
/// [`HookEventKind`] it wants and an optional tool-name matcher, then reacts
/// to each matching fire. The built-in implementation runs a shell command
/// (see `neenee_code`); the trait lives here so the registry and insertion
/// points in `neenee_agent` stay frontend-agnostic.
#[async_trait]
pub trait Hook: Send + Sync {
    fn kind(&self) -> HookEventKind;
    /// Tool-name filter. `None` matches every event; only tool events honour
    /// it. Syntax: a `|`-separated list of exact names (`"Write|Edit"`) when
    /// it matches `[a-zA-Z0-9_|]+`, otherwise a regular expression. Matching
    /// is implemented in `neenee_agent::hooks`.
    fn matcher(&self) -> Option<&str> {
        None
    }
    async fn fire(&self, ctx: &HookContext) -> HookOutcome;
}
