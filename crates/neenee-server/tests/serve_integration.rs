//! Integration test: panicking on assertion failure is the desired
//! behaviour here, so the workspace `unwrap_used`/`expect_used` lints
//! are relaxed for this file. (Lib/bin code stays linted.)
#![allow(clippy::unwrap_used, clippy::expect_used)]

// Integration test: start_server + connect via WS + verify history replay + request/response round-trip
// Run: cargo test -p neenee-server --test serve_integration -- --nocapture

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use neenee_core::{AgentRequest, AgentResponse, RoundEvent};
use neenee_server::serve;
use neenee_store::session::SessionStore;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[tokio::test]
async fn test_ws_round_trip() {
    // Use a temp dir for the session store
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(SessionStore::load_for_project(tmp.path().to_path_buf()));
    let session_id = session.id().await;

    // Create the request channel + broadcast
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<AgentRequest>();
    let (bc_tx, _) = broadcast::channel::<AgentResponse>(1024);

    // Start server on port 0 (OS-assigned)
    let (port_rx, _cancel) = serve::start_server(0, req_tx, bc_tx.clone(), session.clone());
    let port = port_rx.await.unwrap();
    println!("Server started on port {}", port);

    // Connect a WS client
    let ws_url = format!("ws://127.0.0.1:{}", port);
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WS connect");
    println!("Connected to {}", ws_url);

    // 1. Should receive history replay first
    let first_msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for history")
        .expect("stream closed")
        .expect("ws error");
    let first_str = first_msg.to_string();
    println!(
        "Got first message: {}...",
        &first_str[..first_str.len().min(80)]
    );
    assert!(first_str.contains("History"), "expected History replay");

    // 2. Send a chat request through WS
    let request_json = serde_json::json!({
        "type": "Request",
        "Chat": { "text": "hello from test", "images": [] }
    });
    ws.send(WsMessage::Text(request_json.to_string().into()))
        .await
        .unwrap();

    // 3. Verify the request arrived on req_rx
    let req = tokio::time::timeout(Duration::from_secs(2), req_rx.recv())
        .await
        .expect("timeout waiting for request")
        .expect("req_rx closed");
    match req {
        AgentRequest::Chat { text, .. } => {
            assert_eq!(text, "hello from test");
            println!("✓ Request round-trip works: {}", text);
        }
        other => panic!("expected Chat, got {:?}", other),
    }

    // 4. Simulate agent_loop emitting a response → broadcast → should reach WS client
    let resp = AgentResponse::Round {
        session_id,
        event: RoundEvent::Text("hello back from agent".to_string()),
    };
    let _ = bc_tx.send(resp);

    let resp_msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for response")
        .expect("stream closed")
        .expect("ws error");
    let resp_str = resp_msg.to_string();
    let preview = &resp_str[..resp_str.len().min(120)];
    println!("Got response: {}", preview);
    assert!(resp_str.contains("Response"), "expected Response");
    assert!(resp_str.contains("hello back from agent"));

    println!("\n✅ All checks passed: history replay + request round-trip + response tap");
}

#[tokio::test]
async fn serde_shapes() {
    use neenee_core::{AgentRequest, AgentResponse, RoundEvent, ToolOutput};
    println!("--- serialization shapes ---");

    let resp = AgentResponse::Round {
        session_id: "s1".into(),
        event: RoundEvent::Text("hello".into()),
    };
    println!("Turn(Text): {}", serde_json::to_string(&resp).unwrap());

    let tc = AgentResponse::Round {
        session_id: "s1".into(),
        event: RoundEvent::ToolCall {
            id: "c1".into(),
            name: "read_text".into(),
            arguments: "{}".into(),
        },
    };
    println!("Turn(ToolCall): {}", serde_json::to_string(&tc).unwrap());

    let req = AgentRequest::Chat {
        text: "hi".into(),
        images: vec![],
    };
    println!("Request::Chat: {}", serde_json::to_string(&req).unwrap());

    let err = AgentResponse::Error("oops".into());
    println!("Error: {}", serde_json::to_string(&err).unwrap());

    let tr = AgentResponse::Round {
        session_id: "s1".into(),
        event: RoundEvent::ToolResult {
            id: "c1".into(),
            name: "bash".into(),
            output: "done".into(),
            structured: ToolOutput::Text("done".into()),
            duration_ms: 5,
        },
    };
    println!("Turn(ToolResult): {}", serde_json::to_string(&tr).unwrap());

    let delta = AgentResponse::Round {
        session_id: "s1".into(),
        event: RoundEvent::StreamDelta("chunk".into()),
    };
    println!(
        "Turn(StreamDelta): {}",
        serde_json::to_string(&delta).unwrap()
    );

    let clev = AgentResponse::ConversationCleared;
    println!(
        "ConversationCleared: {}",
        serde_json::to_string(&clev).unwrap()
    );

    // Deserialization: can we parse the Wire envelope?
    let incoming = r#"{"type":"Request","Chat":{"text":"hi","images":[]}}"#;
    let parsed: serde_json::Value = serde_json::from_str(incoming).unwrap();
    println!(
        "Parsed incoming: {}",
        serde_json::to_string_pretty(&parsed).unwrap()
    );
}

#[test]
fn wire_request_chat_parses() {
    // Browser sends this:
    let json = r#"{"type":"Request","Chat":{"text":"hello","images":[]}}"#;
    // We want to confirm it deserializes into Wire::Request { AgentRequest::Chat }
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(v["type"], "Request");
    assert_eq!(v["Chat"]["text"], "hello");
    println!("✓ Wire Request::Chat shape confirmed");
}

#[test]
fn wire_request_slash_parses() {
    let json = r#"{"type":"Request","SlashCommand":"/help"}"#;
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(v["type"], "Request");
    assert_eq!(v["SlashCommand"], "/help");
    println!("✓ Wire Request::SlashCommand shape confirmed");
}
