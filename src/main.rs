mod events;
mod gui;
mod memory;
mod preamble;
mod server;
mod session;
mod terminal;
mod tools;

use clap::{Parser, Subcommand};
use session::Session;

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
    if is_server_running().await {
        tracing::info!("found existing server on :8187, connecting as client");
    } else {
        tracing::info!("no server found, starting server");
        start_server_in_background(session_name).await?;
    }
    terminal::TerminalClient::run().await
}

/// Spawn the server in the background and return once it's listening.
async fn start_server_in_background(session_name: Option<String>) -> anyhow::Result<()> {
    let session = Session::new(256, session_name)?;
    tracing::info!("session · {}", session.name());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let app = server::build_router(session);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:8187")
            .await
            .unwrap();
        let _ = ready_tx.send(());
        axum::serve(listener, app).await.unwrap();
    });
    ready_rx.await?;
    Ok(())
}

// ── server mode ────────────────────────────────────────────────

async fn run_server(session_name: Option<String>) -> anyhow::Result<()> {
    let session = Session::new(256, session_name)?;
    tracing::info!("session · {}", session.name());
    server::serve(session).await
}

// ── helpers ────────────────────────────────────────────────────

pub(crate) async fn is_server_running() -> bool {
    tokio::net::TcpStream::connect("127.0.0.1:8187")
        .await
        .is_ok()
}
