//! Concrete LLM provider implementations and the
//! [`build_provider_for_channel`] factory consumed by the orchestration layer.
//!
//! Each transport lives in its own module:
//! - `mock` — trivial in-memory provider used as the default channel.
//! - `openai_compat` — OpenAI-compatible chat completions with native tool
//!   calls and a streaming filter that strips tool-call "echo" text.
//! - `anthropic_compat` — Anthropic-compatible `/messages` (used by
//!   opencode-go's MiniMax/Qwen models and any Anthropic-format relay).
//! - `gemini` — Google Gemini native REST surface.
//! - `registry` — the `OpenAiProviderSpec` table of OpenAI-compatible endpoints
//!   plus the `ANTHROPIC_BUILTIN_MODELS` list backing the configurable
//!   `anthropic` Claude relay, and [`build_provider_for_channel`], which is the
//!   single place that knows
//!   how to turn a [`neenee_core::catalog::Channel`] into a concrete
//!   `dyn Provider`. A keyless OpenAI-compatible relay reaches the same
//!   `OpenAiCompatProvider` as a cloud endpoint (an empty key suppresses the
//!   auth header), so there is no separate local provider module.
//! - `sse` — the shared Server-Sent Events byte-stream decoder every streaming
//!   provider routes through. It reassembles raw bytes across network chunk
//!   boundaries so a multi-byte UTF-8 character split between chunks is not
//!   corrupted into `U+FFFD`.
//!
//! Shared HTTP helpers (`retry_after_ms`, `ensure_success`, `transport_error`,
//! `openai_content`) live here so each provider module can stay focused on its
//! wire format.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod anthropic_compat;
mod gemini;
mod mock;
mod openai_compat;
mod registry;
mod sse;

pub use anthropic_compat::{AnthropicMessagesProvider, Effort, ThinkingConfig, ThinkingMode};
pub use gemini::GeminiProvider;
pub use mock::MockProvider;
pub use openai_compat::OpenAiCompatProvider;
pub use registry::{
    ANTHROPIC_BUILTIN_MODELS, DEEPSEEK_BUILTIN_MODELS, GOOGLE_BUILTIN_MODELS,
    OPENAI_BUILTIN_MODELS, OPENAI_PROVIDER_SPECS, OpenAiProviderSpec, build_provider_for_channel,
    openai_provider_spec,
};

use neenee_core::retryable_error;
use std::time::SystemTime;

/// The default user agent this project sends to providers.
pub const NEENEE_USER_AGENT: &str = concat!("neenee/", env!("CARGO_PKG_VERSION"));

pub(crate) fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
    {
        return Some(milliseconds.max(0.0) as u64);
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<f64>() {
        return Some((seconds.max(0.0) * 1000.0) as u64);
    }
    let parsed = httpdate::parse_http_date(value).ok()?;
    let now = SystemTime::now();
    Some(
        parsed
            .duration_since(now)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
    )
}

pub(crate) async fn ensure_success(
    response: reqwest::Response,
    provider: &str,
) -> Result<reqwest::Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after_ms(response.headers());
    let body = response.text().await.unwrap_or_default();
    let message = format!("{} HTTP {}: {}", provider, status, body);
    if status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error() {
        Err(retryable_error(message, retry_after))
    } else {
        Err(message)
    }
}

/// `io::ErrorKind`s that mean a *transient* connection-layer failure: the peer
/// or an intermediary reset, aborted, truncated, or timed out the connection.
/// These are safe to retry. Logical kinds (`InvalidData`, `PermissionDenied`, …)
/// are deliberately excluded — a retry cannot fix them.
fn is_transient_io_kind(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        kind,
        ConnectionReset
            | ConnectionAborted
            | ConnectionRefused
            | BrokenPipe
            | UnexpectedEof
            | NotConnected
            | TimedOut
    )
}

/// Walk an error's `source()` chain for a `std::io::Error` whose kind is a
/// transient connection failure. reqwest wraps hyper which wraps the underlying
/// `io::Error`, so "connection reset by peer" lives several links down the
/// chain — never on the top-level error — which is exactly why the old
/// top-level `is_connect()`/`is_request()` check could not see it.
fn chain_has_transient_io(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = next {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && is_transient_io_kind(io.kind())
        {
            return true;
        }
        next = err.source();
    }
    false
}

/// Whether a reqwest failure is a transient transport-layer error worth
/// retrying. Supersedes the narrow `is_timeout() || is_connect() || is_request()`
/// check, which only covered connection *establishment*, request *building*, and
/// deadlines — and so silently dropped the most common streaming failure: the
/// connection being reset or truncated *mid-body*, after the response headers
/// arrived (surfaced by reqwest as a body error and, underneath, an
/// `io::ErrorKind::ConnectionReset`/`UnexpectedEof`).
fn is_transient_transport_error(error: &reqwest::Error) -> bool {
    // `is_body()` is the missing piece: a read failure while streaming the
    // response body. The other three retain the original connection/request/
    // deadline coverage.
    if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() {
        return true;
    }
    // Defence in depth: even when reqwest does not categorise it as the above,
    // a reset/abort/truncation is an `io::Error` somewhere in the source chain.
    chain_has_transient_io(error)
}

pub(crate) fn transport_error(provider: &str, error: reqwest::Error) -> String {
    let message = format!("{} transport error: {}", provider, error);
    if is_transient_transport_error(&error) {
        retryable_error(message, None)
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_supports_seconds_and_milliseconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "2.5".parse().unwrap());
        assert_eq!(retry_after_ms(&headers), Some(2_500));

        headers.insert("retry-after-ms", "750".parse().unwrap());
        assert_eq!(retry_after_ms(&headers), Some(750));
    }

    #[test]
    fn transient_io_kinds_are_retryable() {
        use std::io::ErrorKind::*;
        for kind in [
            ConnectionReset,
            ConnectionAborted,
            ConnectionRefused,
            BrokenPipe,
            UnexpectedEof,
            NotConnected,
            TimedOut,
        ] {
            assert!(is_transient_io_kind(kind), "{kind:?} should be transient");
        }
    }

    #[test]
    fn logical_io_kinds_are_not_retryable() {
        use std::io::ErrorKind::*;
        for kind in [InvalidData, InvalidInput, PermissionDenied, NotFound] {
            assert!(
                !is_transient_io_kind(kind),
                "{kind:?} must not be transient"
            );
        }
    }

    #[test]
    fn connection_reset_is_found_deep_in_the_source_chain() {
        // Mirror the reqwest→hyper→io nesting: the reset signal is never on the
        // top-level error, only several `source()` links down.
        #[derive(Debug)]
        struct Wrap(Box<dyn std::error::Error + Send + Sync + 'static>);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "wrapper")
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self.0.as_ref())
            }
        }

        let io = std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        );
        let nested = Wrap(Box::new(Wrap(Box::new(io))));
        assert!(
            chain_has_transient_io(&nested),
            "a reset buried two wrappers deep must still be detected"
        );

        let benign = Wrap(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad utf8",
        )));
        assert!(
            !chain_has_transient_io(&benign),
            "a non-transient io kind must not be flagged"
        );
    }
}
