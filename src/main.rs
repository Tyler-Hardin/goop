mod config;
mod events;
mod gui;
mod mcp;
mod memory;
mod model;
mod preamble;
mod push;
mod server;
mod session;
mod session_state;
mod ssh;
mod terminal;
mod tools;
mod transport;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use session::SessionManager;

#[derive(Parser)]
#[command(name = "goop")]
struct Args {
    /// Resume or create a named session (persisted to disk).
    /// Auto-generated as YYYYMMDD_NNN if not given.
    #[arg(long, short)]
    session: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP + WebSocket server only (headless, no UI).
    Serve,
    /// Launch the desktop GUI (native webview).
    Gui,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,wry=debug,tao=debug,rig_core=warn")
            }),
        )
        .init();

    match args.command {
        Some(Command::Serve) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_server(args.session))?;
        }
        Some(Command::Gui) => gui::run(args.session)?,
        None => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_terminal(args.session))?;
        }
    }

    Ok(())
}

// ── terminal mode ──────────────────────────────────────────────

async fn run_terminal(session_name: Option<String>) -> anyhow::Result<()> {
    let name = session_name.unwrap_or_else(session::next_session_name);

    if is_server_running().await {
        tracing::info!("found existing server on :8187, connecting as client");
    } else {
        tracing::info!("no server found, starting server");
        start_server_in_background().await?;
    }
    terminal::TerminalClient::run(&name).await
}

/// Spawn the server in the background and return once it's listening.
async fn start_server_in_background() -> anyhow::Result<()> {
    let config = config::load_config(None, None)?;
    let push_manager = Arc::new(push::PushManager::new());
    let manager = Arc::new(SessionManager::new(config, Arc::clone(&push_manager)));
    manager.init_global_mcp().await;
    manager.discover().await?;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = ready_tx.send(());
        if let Err(e) = server::serve(manager, push_manager).await {
            tracing::error!("server exited: {e}");
        }
        // If restart was requested, spawn the new server binary.
        // The current process continues running (terminal client
        // will reconnect automatically).
        if server::is_restart_requested() {
            server::spawn_new_binary();
        }
    });
    ready_rx.await?;
    Ok(())
}

// ── server mode ────────────────────────────────────────────────

async fn run_server(session_name: Option<String>) -> anyhow::Result<()> {
    let config = config::load_config(None, None)?;
    let push_manager = Arc::new(push::PushManager::new());
    let manager = Arc::new(SessionManager::new(config, Arc::clone(&push_manager)));
    manager.init_global_mcp().await;
    manager.discover().await?;
    // If the user asked for a specific session, ensure it's loaded.
    if let Some(name) = session_name {
        let session = manager.get_or_create(name).await?;
        tracing::info!("session · {}", session.name());
    }
    server::serve(manager, push_manager).await?;

    // If restart was requested, spawn the new server and exit.
    // The new process takes over the :8187 port.
    if server::is_restart_requested() {
        server::spawn_new_binary();
        std::process::exit(0);
    }

    Ok(())
}

// ── helpers ────────────────────────────────────────────────────

pub(crate) async fn is_server_running() -> bool {
    tokio::net::TcpStream::connect("127.0.0.1:8187")
        .await
        .is_ok()
}
