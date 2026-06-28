//! WebSocket transport for the "hot-attach" serve mode (ADR-0037 §7).
//!
//! When `/serve` is invoked from the running TUI, [`start_server`] spawns a
//! TCP listener that accepts WebSocket connections. Each connection:
//!
//! 1. Receives the session's full transcript history (so a freshly-opened
//!    browser sees prior context, not just live events from connect onward).
//! 2. Streams every subsequent [`AgentResponse`] from the broadcast channel
//!    (the TUI listener task taps each response into it).
//! 3. Reads inbound [`AgentRequest`]s from the WebSocket and feeds them into
//!    the same `req_tx` the TUI uses — so a browser request and a TUI
//!    keystroke are indistinguishable to `agent_loop`.
//!
//! The wire format is newline-delimited JSON: one `serde_json`-serialized
//! `AgentRequest` or `AgentResponse` per WebSocket text frame.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use neenee_core::{AgentRequest, AgentResponse, Message};
use neenee_store::session::SessionStore;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// The wire envelope. Each WebSocket text frame is one of these, JSON-encoded.
/// Inbound (browser → server) is always [`Wire::Request`]; outbound
/// (server → browser) is [`Wire::Response`] or [`Wire::History`] (sent once
/// at connect, before any live responses).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
enum Wire {
    Request {
        #[serde(flatten)]
        request: AgentRequest,
    },
    Response {
        #[serde(flatten)]
        response: AgentResponse,
    },
    /// Full transcript replay, sent once on connect so the browser catches up
    /// on everything that happened before it joined.
    History { messages: Vec<Message> },
}

/// Spawn the WebSocket server. Returns immediately; the listener runs as a
/// detached tokio task that lives until the process exits or the broadcast
/// channel is dropped (which happens when `/serve stop` clears the tap).
///
/// - `port`: the TCP port to listen on.
/// - `req_tx`: the existing agent-loop request channel. Browser requests are
///   fed in here alongside TUI requests.
/// - `events`: the broadcast channel the TUI listener taps responses into.
///   Each WS connection subscribes to this.
/// - `session`: the session store, used to replay transcript history on connect.
pub fn start_server(
    port: u16,
    req_tx: mpsc::UnboundedSender<AgentRequest>,
    events: broadcast::Sender<AgentResponse>,
    session: Arc<SessionStore>,
) -> (
    tokio::sync::oneshot::Receiver<u16>,
    tokio_util::sync::CancellationToken,
) {
    let (actual_port_tx, actual_port_rx) = tokio::sync::oneshot::channel::<u16>();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => {
                let actual = l.local_addr().map(|a| a.port()).unwrap_or(port);
                let _ = actual_port_tx.send(actual);
                tracing::info!(%addr, actual_port = actual, "neenee serve: WebSocket listener started");
                l
            }
            Err(e) => {
                tracing::error!(%addr, error = %e, "neenee serve: failed to bind");
                return;
            }
        };
        loop {
            // When `/serve stop` cancels the token, stop accepting.
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    tracing::info!("neenee serve: cancelled, stopping listener");
                    break;
                }
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(conn) => conn,
                        Err(e) => {
                            tracing::warn!(error = %e, "neenee serve: accept failed");
                            continue;
                        }
                    };
            let req_tx = req_tx.clone();
            let events = events.clone();
            let session = session.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, req_tx, events, session).await {
                    tracing::warn!(%peer_addr, error = %e, "neenee serve: connection ended");
                }
            });
                }
            }
        }
    });
    (actual_port_rx, cancel)
}

/// Handle a single WebSocket connection: replay history, then bridge
/// broadcast → WS and WS → req_tx concurrently.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    req_tx: mpsc::UnboundedSender<AgentRequest>,
    events: broadcast::Sender<AgentResponse>,
    session: Arc<SessionStore>,
) -> Result<(), String> {
    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| format!("ws handshake: {e}"))?;
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    // 1. Replay transcript history so the browser sees prior context.
    let messages = session.full_transcript().await;
    let history = serde_json::to_string(&Wire::History { messages })
        .map_err(|e| format!("serialize history: {e}"))?;
    ws_sink
        .send(WsMessage::Text(history.into()))
        .await
        .map_err(|e| format!("send history: {e}"))?;

    // 2. Subscribe to the live response broadcast.
    let mut rx = events.subscribe();

    // 3. Bridge both directions concurrently. The task ends when either
    //    direction closes (browser disconnects or server stops).
    loop {
        tokio::select! {
            // broadcast → browser
            resp = rx.recv() => {
                match resp {
                    Ok(resp) => {
                        let text = serde_json::to_string(&Wire::Response { response: resp })
                            .map_err(|e| format!("serialize response: {e}"))?;
                        ws_sink.send(WsMessage::Text(text.into())).await
                            .map_err(|e| format!("ws send: {e}"))?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "neenee serve: client lagged, skipping");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break; // server stopped
                    }
                }
            }
            // browser → agent_loop
            msg = ws_source.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        match serde_json::from_str::<Wire>(&text) {
                            Ok(Wire::Request { request }) => {
                                let _ = req_tx.send(request);
                            }
                            Ok(_) => {
                                // Ignore non-request inbound messages.
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "neenee serve: bad request json");
                            }
                        }
                    }
                    Some(Ok(_)) => {} // ignore binary/ping/pong
                    Some(Err(e)) => {
                        return Err(format!("ws recv: {e}"));
                    }
                    None => {
                        break; // browser disconnected
                    }
                }
            }
        }
    }
    Ok(())
}
