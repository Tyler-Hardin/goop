use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::events::PromptSource;
use crate::session::Session;

const PAGE: &str = include_str!("../assets/index.html");

/// Launch the axum HTTP + WebSocket server.
/// Binds to 127.0.0.1:8187 — safe behind an nginx reverse proxy.
pub async fn serve(session: Arc<Session>) -> anyhow::Result<()> {
    let state = Arc::new(ServerState { session });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8187").await?;
    println!("web server on http://127.0.0.1:8187");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── state ───────────────────────────────────────────────────────

struct ServerState {
    session: Arc<Session>,
}

// ── routes ──────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

/// Upgrade to WebSocket after validating the Origin header.
/// Rejects requests from unexpected origins to prevent CSWSH.
async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
) -> Response {
    // Only allow requests whose Origin matches the Host we're proxied behind.
    // When nginx proxies, the browser sends Origin = https://your.domain,
    // and nginx forwards Host = your.domain.  We check they agree.
    if let (Some(origin), Some(host)) = (
        headers.get("origin").and_then(|v| v.to_str().ok()),
        headers.get("host").and_then(|v| v.to_str().ok()),
    ) {
        // Strip scheme from origin for comparison (origin = "https://foo.com", host = "foo.com")
        let origin_host = origin
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        if origin_host != host {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

// ── websocket ───────────────────────────────────────────────────

async fn handle_socket(ws: WebSocket, state: Arc<ServerState>) {
    let (mut tx, mut rx) = ws.split();

    // Subscribe with full history replay so the phone sees the whole chat.
    let mut events = state.session.subscribe_all().await;

    // Spawn a task that reads session events and writes them to the socket.
    let mut send_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let json = serde_json::to_string(&event).unwrap();
                    if tx
                        .send(axum::extract::ws::Message::Text(json.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Read incoming messages (prompts) from the client.
    let session = Arc::clone(&state.session);
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(req) = serde_json::from_str::<ClientMessage>(&text) {
                        match req {
                            ClientMessage::Prompt { content } => {
                                session.submit(content, PromptSource::Web);
                            }
                            ClientMessage::Cancel => {
                                session.cancel();
                            }
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // If either task ends, abort the other.
    tokio::select! {
        _ = &mut send_task => { recv_task.abort(); }
        _ = &mut recv_task => { send_task.abort(); }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "prompt")]
    Prompt { content: String },
    #[serde(rename = "cancel")]
    Cancel,
}
