//! Concrete LLM provider implementations and the
//! [`build_provider_for_channel`] factory consumed by the orchestration layer.
//!
//! Each transport lives in its own module:
//! - `mock` — trivial in-memory provider used as the default channel.
//! - `openai_compat` — OpenAI-compatible chat completions with native tool
//!   calls and a streaming filter that strips tool-call "echo" text.
//! - `gemini` — Google Gemini native REST surface.
//! - `llama` — local llama.cpp / llama-server HTTP provider.
//! - `registry` — `OpenAiProviderSpec` table of OpenAI-compatible endpoints
//!   and [`build_provider_for_channel`], which is the single place that knows
//!   how to turn a [`neenee_core::catalog::Channel`] into a concrete
//!   `dyn Provider`.
//!
//! Shared HTTP helpers (`retry_after_ms`, `ensure_success`, `transport_error`,
//! `openai_content`) live here so each provider module can stay focused on its
//! wire format.

mod gemini;
mod llama;
mod mock;
mod openai_compat;
mod registry;

pub use gemini::GeminiProvider;
pub use llama::LlamaServerProvider;
pub use mock::MockProvider;
pub use openai_compat::OpenAiCompatProvider;
pub use registry::{
    build_provider_for_channel, openai_provider_spec, OpenAiProviderSpec, OPENAI_PROVIDER_SPECS,
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

pub(crate) fn transport_error(provider: &str, error: reqwest::Error) -> String {
    let message = format!("{} transport error: {}", provider, error);
    if error.is_timeout() || error.is_connect() || error.is_request() {
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
}
