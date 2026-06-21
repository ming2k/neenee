//! Harness error types and the classification of raw provider error strings.
//!
//! The `Provider`/`Tool` traits return opaque `String` errors (HTTP and
//! transport failures are naturally just text). Those strings are classified
//! into the typed [`HarnessError`] exactly once, at the harness boundary, so
//! the turn loop and its callers can match outcomes by variant instead of by
//! fragile string compare.

use serde::{Deserialize, Serialize};

const RETRYABLE_ERROR_PREFIX: &str = "[NEENEE_RETRYABLE]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryableError {
    pub message: String,
    pub retry_after_ms: Option<u64>,
}

pub fn retryable_error(message: impl Into<String>, retry_after_ms: Option<u64>) -> String {
    let error = RetryableError {
        message: message.into(),
        retry_after_ms,
    };
    format!(
        "{}{}",
        RETRYABLE_ERROR_PREFIX,
        serde_json::to_string(&error).unwrap_or_else(|_| "{}".to_string())
    )
}

pub fn parse_retryable_error(error: &str) -> Option<RetryableError> {
    serde_json::from_str(error.strip_prefix(RETRYABLE_ERROR_PREFIX)?).ok()
}

pub fn public_error_message(error: &str) -> String {
    parse_retryable_error(error)
        .map(|retry| retry.message)
        .unwrap_or_else(|| error.to_string())
}

/// Heuristic: does this provider error indicate the request exceeded the
/// model's context window? Used to trigger a compaction-and-retry instead of a
/// plain failure.
pub fn is_context_overflow(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "context_window",
        "maximum context",
        "too many tokens",
        "token limit",
    ]
    .iter()
    .any(|pattern| error.contains(pattern))
}

/// A typed harness error.
///
/// Replaces the previous practice of smuggling control signals through error
/// *string contents* — a retryable JSON prefix, substring scans for context
/// overflow, and an `"Interrupted"` sentinel — so the turn loop and its callers
/// match outcomes exhaustively by variant instead of by fragile string compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    /// A transient failure (rate limit, overload, timeout) that may be retried.
    Retryable {
        message: String,
        retry_after_ms: Option<u64>,
    },
    /// The request exceeded the model's context window.
    ContextOverflow(String),
    /// The turn was cancelled by the user.
    Interrupted,
    /// The turn hit the tool-round budget cap. This is a **planned stop**,
    /// not a runtime failure: the agent ran out of tool budget rather than
    /// crashing. Surfaced distinctly so the UI can render it as a recoverable
    /// notice (with a "continue" affordance) instead of a red error.
    TurnLimitReached { rounds: usize },
    /// Any other terminal failure; the message is user-facing.
    Other(String),
}

impl HarnessError {
    /// Classify a raw provider/transport error string into a typed error. This
    /// is the single place the legacy string encoding is decoded.
    pub fn classify(error: String) -> Self {
        if let Some(retry) = parse_retryable_error(&error) {
            return Self::Retryable {
                message: retry.message,
                retry_after_ms: retry.retry_after_ms,
            };
        }
        if is_context_overflow(&error) {
            return Self::ContextOverflow(error);
        }
        Self::Other(error)
    }
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable { message, .. }
            | Self::ContextOverflow(message)
            | Self::Other(message) => write!(f, "{message}"),
            Self::Interrupted => write!(f, "Interrupted"),
            Self::TurnLimitReached { rounds } => write!(
                f,
                "Turn paused after {rounds} tool rounds. Refine the goal or continue with /loop."
            ),
        }
    }
}

impl std::error::Error for HarnessError {}

/// Raw provider/transport error strings classify into a typed error when
/// propagated with `?` inside the turn loop.
impl From<String> for HarnessError {
    fn from(error: String) -> Self {
        Self::classify(error)
    }
}
