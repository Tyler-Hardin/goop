use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{Notify, broadcast};

use crate::events::PromptSource;
use crate::session::{Session, SessionManager};

const PAGE: &str = include_str!("../assets/index.html");
const MANIFEST: &str = include_str!("../assets/manifest.json");
const SERVICE_WORKER: &str = include_str!(concat!(env!("OUT_DIR"), "/sw.js"));
const ICON: &[u8] = include_bytes!("../assets/goop_icon_full.png");

// ── restart machinery ──────────────────────────────────────────────

/// Set to true by the `restart` tool.  The session drain loop checks
/// this after each prompt completes; when true it notifies the shutdown
/// signal and the server closes its TCP listener gracefully.
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Fires when the server should shut down (restart or Ctrl+C).
static SHUTDOWN_NOTIFY: OnceLock<Notify> = OnceLock::new();

fn shutdown_notify() -> &'static Notify {
    SHUTDOWN_NOTIFY.get_or_init(Notify::new)
}

/// Called by the `restart` tool.  Sets a flag; the current prompt
/// completes normally, then the server shuts down gracefully.
pub fn trigger_restart() {
    RESTART_REQUESTED.store(true, Ordering::SeqCst);
}

/// Checked by the session drain loop after each prompt finishes.
pub fn is_restart_requested() -> bool {
    RESTART_REQUESTED.load(Ordering::SeqCst)
}

/// Called by the session drain loop to begin the graceful shutdown.
pub fn notify_shutdown() {
    shutdown_notify().notify_one();
}

/// Returns a future that resolves when the server should shut down.
pub async fn shutdown_signal() {
    shutdown_notify().notified().await;
}

/// Spawn the newly-compiled binary as a detached child.
///
/// Checks `current_exe`, `target/debug/goop`, and `target/release/goop`,
/// picks the one with the newest modification time, and spawns it.
/// Returns `true` on success.  Does **not** call `process::exit` — the
/// caller decides whether to exit after spawning.
pub fn spawn_new_binary() -> bool {
    let args: Vec<String> = std::env::args().collect();

    let candidates: [std::path::PathBuf; 3] = [
        std::env::current_exe().unwrap_or_else(|_| "goop".into()),
        std::env::current_dir()
            .unwrap_or_default()
            .join("target/debug/goop"),
        std::env::current_dir()
            .unwrap_or_default()
            .join("target/release/goop"),
    ];

    // Pick the newest candidate that actually exists.
    let newest = candidates
        .iter()
        .filter_map(|p| {
            std::fs::metadata(p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (p, t))
        })
        .max_by_key(|(_, t)| *t);

    let exe = match newest {
        Some((exe, _)) => exe,
        None => {
            tracing::error!("could not find goop binary to restart — tried: {candidates:?}");
            return false;
        }
    };

    match std::process::Command::new(exe)
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(_) => {
            tracing::info!("new server spawned from {}", exe.display());
            true
        }
        Err(e) => {
            tracing::error!("failed to spawn {}: {e}", exe.display());
            false
        }
    }
}

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
///
/// Returns when the shutdown signal fires (restart tool or Ctrl+C).
pub async fn serve(manager: Arc<SessionManager>) -> anyhow::Result<()> {
    let app = build_router(manager);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8187").await?;
    tracing::info!("web server on http://127.0.0.1:8187");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

// ── state ───────────────────────────────────────────────────────

struct ServerState {
    manager: Arc<SessionManager>,
}

// ── routes ──────────────────────────────────────────────────────

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE))
}

// ── PWA static assets ────────────────────────────────────────────

async fn manifest() -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 2],
    &'static str,
) {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        MANIFEST,
    )
}

async fn service_worker() -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 2],
    &'static str,
) {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-store"),
        ],
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
