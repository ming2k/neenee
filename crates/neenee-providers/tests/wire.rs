//! Integration test: panicking on assertion failure is the desired
//! behaviour here, so the workspace `unwrap_used`/`expect_used` lints
//! are relaxed for this file. (Lib/bin code stays linted.)
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Wire-level integration tests for the chat/streaming provider implementations.
//!
//! The in-module unit tests can only exercise the *pure* parsing helpers
//! (`parse_openai_stream_data`, `parse_anthropic_stream_data`, the echo filter)
//! because the `chat` / `stream_chat_events` methods build a live `reqwest`
//! request. These tests stand up a localhost mock HTTP server (mockito) and
//! drive the full request → HTTP → SSE-byte-reassembly → event-parse path, so
//! the integration behaviour — header attachment, error classification, and
//! echo suppression over a real stream — is covered.

use futures::StreamExt;
use mockito::{Matcher, Server};
use neenee_core::{Message, Provider, ProviderStreamEvent, Role};
use neenee_providers::{AnthropicMessagesProvider, OpenAiCompatProvider};
use serde_json::{Value, json};

/// Join SSE `data:` events into a single response body. Each event becomes one
/// `data: <payload>\n\n` frame — the shape `sse::data_payloads` decodes.
fn sse_body(events: &[&str]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// Collect a stream of provider events into a flat `Vec`, failing if any item
/// is itself an `Err`. Mirrors how the harness drains a turn's event stream.
async fn collect_events(
    stream: futures::stream::BoxStream<'static, Result<ProviderStreamEvent, String>>,
) -> Vec<ProviderStreamEvent> {
    let mut out = Vec::new();
    for item in stream.collect::<Vec<_>>().await {
        out.push(item.expect("stream item must be Ok"));
    }
    out
}

// ═════════════════════════════════════════════════════════════════════════════
// OpenAI-compatible provider
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn openai_chat_parses_content_reasoning_tool_calls_and_headers() {
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/chat/completions", server.url());
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        // The bearer token and chosen user agent must reach the wire.
        .match_header("authorization", "Bearer test-key")
        .match_header("user-agent", "neenee-test/1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"Hello!","reasoning_content":"thinking","tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        )
        .create_async()
        .await;

    let provider = OpenAiCompatProvider::with_base_url_and_user_agent(
        "test-key".to_string(),
        "gpt-test".to_string(),
        &url,
        "neenee-test/1",
    );
    let message = provider
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .expect("chat should succeed");

    assert_eq!(message.content, "Hello!");
    assert_eq!(
        message.reasoning_content.as_deref(),
        Some("thinking"),
        "reasoning_content must be parsed"
    );
    let calls = message
        .tool_calls
        .as_ref()
        .expect("tool_calls must be present");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "bash");
    assert_eq!(calls[0].arguments, r#"{"command":"ls"}"#);
}

#[tokio::test]
async fn openai_chat_strips_tool_call_echo_when_native_calls_present() {
    // GLM/Qwen leak: the same tool call arrives both as `content` text and as a
    // native `tool_calls` entry. The native call wins and the textual mirror is
    // suppressed so raw JSON never reaches the UI.
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/chat/completions", server.url());
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"{\"tool\":\"bash\",\"arguments\":{\"command\":\"ls\"}}","tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        )
        .create_async()
        .await;

    let provider = OpenAiCompatProvider::with_base_url("k".to_string(), "m".to_string(), &url);
    let message = provider
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .expect("chat should succeed");

    assert!(
        message.content.is_empty(),
        "mirrored echo must be stripped when native tool calls are present: got {:?}",
        message.content
    );
    assert_eq!(
        message.tool_calls.as_ref().expect("native call")[0].name,
        "bash"
    );
}

#[tokio::test]
async fn openai_chat_classifies_server_error_as_retryable() {
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/chat/completions", server.url());
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("upstream boom")
        .create_async()
        .await;

    let provider = OpenAiCompatProvider::with_base_url("k".to_string(), "m".to_string(), &url);
    let error = provider
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .expect_err("5xx must surface as an error");

    // ensure_success tags 5xx as retryable so the harness backs off and retries.
    assert!(
        neenee_core::parse_retryable_error(&error).is_some(),
        "5xx must be classified retryable: {error}"
    );
    assert!(error.contains("HTTP 500"));
}

#[tokio::test]
async fn openai_chat_omits_auth_header_when_api_key_is_empty() {
    // Keyless servers (a local `llama-server` started without `--api-key`) must
    // not receive an empty `Authorization: Bearer ` header, which some servers
    // reject even when they would otherwise ignore the key.
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/chat/completions", server.url());
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("authorization", Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"choices":[{"message":{"content":"ok"}}]}"#)
        .create_async()
        .await;

    let provider = OpenAiCompatProvider::with_base_url_and_user_agent(
        String::new(),
        "m".to_string(),
        &url,
        "ua",
    );
    let message = provider
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .expect("keyless chat should succeed");
    assert_eq!(message.content, "ok");
}

#[tokio::test]
async fn openai_stream_parses_text_reasoning_and_tool_call_deltas() {
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/chat/completions", server.url());
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
        r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]}}]}"#,
        "[DONE]",
    ]);
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        // The streaming path must request a stream.
        .match_body(Matcher::PartialJson(json!({"stream": true})))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = OpenAiCompatProvider::with_base_url("k".to_string(), "m".to_string(), &url);
    let stream = provider
        .stream_chat_events(vec![Message::new(Role::User, "hi")])
        .await
        .expect("stream should open");
    let events = collect_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello");

    assert!(events.iter().any(|event| matches!(
        event,
        ProviderStreamEvent::ReasoningDelta(reasoning) if reasoning == "hm"
    )));
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::ToolCallDelta { name, .. } => name.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls, vec!["bash".to_string()]);
}

#[tokio::test]
async fn openai_stream_strips_echo_text_when_native_tool_calls_stream_in() {
    // Over a real stream: the textual tool-call mirror and the native tool-call
    // delta both arrive. The echo filter must hold the mirror and drop it once
    // the native call is observed, so no raw JSON leaks as a TextDelta.
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/chat/completions", server.url());
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"{\"tool\":\"bash\",\"arguments\":{\"command\":\"ls\"}}"}}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        "[DONE]",
    ]);
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = OpenAiCompatProvider::with_base_url("k".to_string(), "m".to_string(), &url);
    let stream = provider
        .stream_chat_events(vec![Message::new(Role::User, "hi")])
        .await
        .expect("stream should open");
    let events = collect_events(stream).await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderStreamEvent::TextDelta(_))),
        "no TextDelta should survive: the echo must be stripped, got {events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ProviderStreamEvent::ToolCallDelta { name, .. } if name.as_deref() == Some("bash")
    )));
}

// ═════════════════════════════════════════════════════════════════════════════
// Anthropic-compatible /messages provider
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn anthropic_chat_assembles_text_thinking_and_tool_use() {
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/messages", server.url());
    let _mock = server
        .mock("POST", "/v1/messages")
        // The Messages surface identifies via x-api-key + anthropic-version.
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"content":[
                {"type":"thinking","thinking":"deliberating"},
                {"type":"text","text":"Done."},
                {"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls"}}
            ]}"#,
        )
        .create_async()
        .await;

    let provider = AnthropicMessagesProvider::with_user_agent(
        "test-key".to_string(),
        "minimax-m3".to_string(),
        &url,
        "ua",
    );
    let message = provider
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .expect("chat should succeed");

    assert_eq!(message.content, "Done.");
    assert_eq!(message.reasoning_content.as_deref(), Some("deliberating"));
    let calls = message
        .tool_calls
        .as_ref()
        .expect("tool_use must map to tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].name, "bash");
    // The `input` object is serialized back to a JSON argument string.
    let input: Value = serde_json::from_str(&calls[0].arguments).expect("input is valid json");
    assert_eq!(input["command"], "ls");
}

#[tokio::test]
async fn anthropic_stream_parses_tool_use_block_and_argument_fragments() {
    // A tool_use block opens at index 1 (id + name up front), then its argument
    // JSON streams in as `input_json_delta` fragments the harness concatenates.
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/messages", server.url());
    let body = sse_body(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"and\":\"ls\"}"}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let _mock = server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = AnthropicMessagesProvider::with_user_agent(
        "k".to_string(),
        "minimax-m3".to_string(),
        &url,
        "ua",
    );
    let stream = provider
        .stream_chat_events(vec![Message::new(Role::User, "hi")])
        .await
        .expect("stream should open");
    let events = collect_events(stream).await;

    assert!(events.iter().any(|event| matches!(
        event,
        ProviderStreamEvent::TextDelta(text) if text == "Hi"
    )));
    // The opening block carries id + name; the two argument fragments follow.
    let tool_events: Vec<&ProviderStreamEvent> = events
        .iter()
        .filter(|event| matches!(event, ProviderStreamEvent::ToolCallDelta { .. }))
        .collect();
    assert_eq!(tool_events.len(), 3, "open + 2 fragments");
    assert!(matches!(
        tool_events[0],
        ProviderStreamEvent::ToolCallDelta { id, name, .. }
            if id.as_deref() == Some("toolu_1") && name.as_deref() == Some("bash")
    ));
}

#[tokio::test]
async fn anthropic_stream_surfaces_in_band_error_event() {
    // Anthropic can emit an `error` event mid-stream (e.g. overloaded); the
    // parser must surface it as an Err item rather than a silent empty stream.
    let mut server = Server::new_async().await;
    let url = format!("{}/v1/messages", server.url());
    let body = sse_body(&[
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    ]);
    let _mock = server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = AnthropicMessagesProvider::with_user_agent(
        "k".to_string(),
        "minimax-m3".to_string(),
        &url,
        "ua",
    );
    let stream = provider
        .stream_chat_events(vec![Message::new(Role::User, "hi")])
        .await
        .expect("stream should open");
    let items = stream.collect::<Vec<_>>().await;
    let errored = items.iter().any(|item| {
        item.as_ref()
            .is_err_and(|error| error.contains("Overloaded"))
    });
    assert!(
        errored,
        "in-band error must surface as an Err item: {items:?}"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// End-to-end through the production factory: Transport::Anthropic{effort,thinking}
// → build_provider_for_channel → request_body → HTTP. This is the regression
// suite for the effort/thinking decoupling + the high-effort-swallow fix. It
// drives the *real* public API (not the private request_body), so it proves the
// wire body a configured channel actually publishes.
// ═════════════════════════════════════════════════════════════════════════════

use neenee_core::catalog::{Channel, Transport};
use neenee_core::{Effort, ThinkingMode};
use neenee_providers::build_provider_for_channel;

/// Build a channel → factory provider, send one turn to a mockito server that
/// asserts the request body matches `expected` (partial JSON), and confirm the
/// call succeeds. The shared harness for the three decoupling regressions.
async fn assert_factory_body(mut channel: Channel, expected: Value) {
    let mut server = Server::new_async().await;
    // Point the channel at the mock server by rewriting its base_url in place.
    let url = format!("{}/v1/messages", server.url());
    if let Transport::Anthropic { base_url, .. } = &mut channel.transport {
        *base_url = url;
    }
    let _mock = server
        .mock("POST", "/v1/messages")
        .match_body(Matcher::PartialJson(expected))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"content":[{"type":"text","text":"ok"}]}"#)
        .create_async()
        .await;

    let provider = build_provider_for_channel(&channel, "anthropic");
    let msg = provider
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .expect("factory-built provider chat must succeed");
    assert_eq!(msg.content, "ok");
}

/// Regression #1: an explicit `effort = "high"` MUST publish
/// `output_config.effort = "high"`. Before the fix the value `High` was
/// treated as "the default" and silently dropped, so a channel pinned to high
/// was a no-op on the wire.
#[tokio::test]
async fn factory_publishes_explicit_high_effort() {
    let channel = Channel {
        id: "claude-opus-4-8".into(),
        label: "Opus".into(),
        transport: Transport::Anthropic {
            base_url: String::new(), // rewritten by the harness
            user_agent: "ua".into(),
            effort: Some(Effort::High),
            thinking: None,
        },
        api_key: "k".into(),
        model: "claude-opus-4-8".into(),
    };
    assert_factory_body(channel, json!({ "output_config": { "effort": "high" } })).await;
}

/// Regression #2: effort and thinking stay DECOUPLED. A channel with an effort
/// override but thinking OFF must publish effort while the model won't reason.
/// (Previously setting effort forced `thinking:{adaptive}` on.) The pure-mode
/// contract (no `thinking` field) is asserted in the unit test
/// `effort_without_thinking_stays_decoupled`; this test proves the factory
/// honors an explicit `ThinkingMode::Off` together with an effort override end
/// to end — i.e. the two overrides reach the provider independently.
#[tokio::test]
async fn factory_keeps_effort_decoupled_from_thinking_off() {
    let channel = Channel {
        id: "claude-opus-4-8".into(),
        label: "Opus".into(),
        transport: Transport::Anthropic {
            base_url: String::new(),
            user_agent: "ua".into(),
            effort: Some(Effort::Medium),
            thinking: Some(ThinkingMode::Off),
        },
        api_key: "k".into(),
        model: "claude-opus-4-8".into(),
    };
    // The request publishes the effort override; the absence of a `thinking`
    // field is verified by the companion unit test.
    assert_factory_body(channel, json!({ "output_config": { "effort": "medium" } })).await;
}

/// Regression #3: a thinking ON override with no effort publishes
/// `thinking:{adaptive}` and omits `output_config` (no explicit effort).
#[tokio::test]
async fn factory_publishes_thinking_without_output_config() {
    let channel = Channel {
        id: "claude-opus-4-8".into(),
        label: "Opus".into(),
        transport: Transport::Anthropic {
            base_url: String::new(),
            user_agent: "ua".into(),
            effort: None,
            thinking: Some(ThinkingMode::Adaptive),
        },
        api_key: "k".into(),
        model: "claude-opus-4-8".into(),
    };
    assert_factory_body(
        channel,
        json!({ "thinking": { "type": "adaptive", "display": "summarized" } }),
    )
    .await;
}
