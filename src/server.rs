use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::events::PromptSource;
use crate::session::{Session, SessionManager};

const PAGE: &str = include_str!("../assets/index.html");
const MANIFEST: &str = include_str!("../assets/manifest.json");
const SERVICE_WORKER: &str = include_str!("../assets/sw.js");
const ICON: &[u8] = include_bytes!("../assets/goop_icon_full.png");

/// Build the axum router (exposed so GUI mode can bind the listener
/// synchronously before opening the webview).
pub fn build_router(manager: Arc<SessionManager>) -> Router {
    let state = Arc::new(ServerState { manager });
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{name}", delete(delete_session))
        .route("/manifest.json", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .with_state(state)
}

/// Launch the axum HTTP + WebSocket server.
/// Binds to 127.0.0.1:8187 — safe behind an nginx reverse proxy.
pub async fn serve(manager: Arc<SessionManager>) -> anyhow::Result<()> {
    let app = build_router(manager);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8187").await?;
    tracing::info!("web server on http://127.0.0.1:8187");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── state ───────────────────────────────────────────────────────

struct ServerState {
    manager: Arc<SessionManager>,
}

// ── routes ──────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

// ── PWA static assets ────────────────────────────────────────────

async fn manifest() -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST,
    )
}

async fn service_worker() -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/javascript")],
        SERVICE_WORKER,
    )
}

async fn icon_192() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], ICON)
}

async fn icon_512() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], ICON)
}

// ── REST API ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateSessionBody {
    name: Option<String>,
}

async fn list_sessions(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let names = state.manager.list().await;
    axum::Json(names)
}

async fn create_session(
    State(state): State<Arc<ServerState>>,
    axum::Json(body): axum::Json<CreateSessionBody>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let session = state
        .manager
        .create(body.name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(axum::Json(serde_json::json!({ "name": session.name() })))
}

async fn delete_session(
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    state.manager.delete(&name).await;
    axum::Json(serde_json::json!({ "deleted": true })).into_response()
}

// ── websocket ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct WsParams {
    /// Optional session name. If absent, a new auto-named session is
    /// created for this connection.
    session: Option<String>,
}

/// Upgrade to WebSocket after validating the Origin header.
/// Routes to the correct session based on `?session=<name>`.
async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<WsParams>,
    State(state): State<Arc<ServerState>>,
) -> Response {
    // Only allow requests whose Origin matches the Host we're proxied behind,
    // or whose origin is null (GUI webview with `with_html`).
    if let (Some(origin), Some(host)) = (
        headers.get("origin").and_then(|v| v.to_str().ok()),
        headers.get("host").and_then(|v| v.to_str().ok()),
    ) {
        if origin == "null" {
            return resolve_and_upgrade(ws, state, params.session);
        }
        let origin_host = origin
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        if origin_host != host {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    resolve_and_upgrade(ws, state, params.session)
}

/// Resolve (or create) the requested session, then upgrade.
fn resolve_and_upgrade(
    ws: WebSocketUpgrade,
    state: Arc<ServerState>,
    session_name: Option<String>,
) -> Response {
    let manager = Arc::clone(&state.manager);
    ws.on_upgrade(move |socket| async move {
        let session = match session_name {
            Some(name) => match manager.get_or_create(name).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to get or create session: {e}");
                    return;
                }
            },
            None => match manager.create(None).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to create session: {e}");
                    return;
                }
            },
        };
        handle_socket(socket, session).await;
    })
}

async fn handle_socket(ws: WebSocket, session: Arc<Session>) {
    let (mut tx, mut rx) = ws.split();

    // Subscribe with full history replay so the client sees the whole chat.
    let mut events = session.subscribe_all().await;

    // Spawn a task that reads session events and writes them to the socket.
    let mut send_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let json = serde_json::to_string(&event)
                        .expect("SessionEvent serialization should never fail");
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
    let session = Arc::clone(&session);
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
                                session.cancel().await;
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
