mod events;
mod server;
mod session;
mod terminal;
mod tools;

use clap::{Parser, Subcommand};
use session::Session;

#[derive(Parser)]
#[command(name = "goop")]
struct Args {
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rig_core=warn")),
        )
        .init();

    match args.command {
        Some(Command::Serve) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_server())?;
        }
        Some(Command::Gui) => run_gui()?,
        None => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_terminal())?;
        }
    }

    Ok(())
}

// ── terminal mode ──────────────────────────────────────────────

async fn run_terminal() -> anyhow::Result<()> {
    if is_server_running().await {
        tracing::info!("found existing server on :8187, connecting as client");
    } else {
        tracing::info!("no server found, starting server");
        start_server_in_background().await?;
    }
    terminal::TerminalClient::run().await
}

/// Spawn the server in the background and return once it's listening.
async fn start_server_in_background() -> anyhow::Result<()> {
    let session = Session::new(256)?;
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

// ── GUI mode ───────────────────────────────────────────────────

fn run_gui() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;

    if rt.block_on(is_server_running()) {
        tracing::info!("found existing server on :8187, opening webview as client");
        run_gui_client()
    } else {
        tracing::info!("no server found, starting primary");
        run_gui_primary(rt)
    }
}

/// GUI mode when we own the session + server.
fn run_gui_primary(rt: tokio::runtime::Runtime) -> anyhow::Result<()> {
    let session = rt.block_on(async { Session::new(256) })?;
    let app = server::build_router(session);

    // Bind the TCP listener synchronously on the main thread so that
    // the server is guaranteed to be listening before the webview loads.
    let listener = std::net::TcpListener::bind("127.0.0.1:8187")?;
    listener.set_nonblocking(true)?;
    let listener = rt.block_on(async { tokio::net::TcpListener::from_std(listener) })?;

    // Signal when the server is truly ready to accept requests.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);

    rt.spawn(async move {
        let _ = ready_tx.send(());
        tracing::info!("web server on http://127.0.0.1:8187");
        axum::serve(listener, app).await.unwrap();
    });

    let _ = ready_rx.recv();

    open_webview()
}

/// GUI mode when a server is already running — just open the webview.
fn run_gui_client() -> anyhow::Result<()> {
    open_webview()
}

fn open_webview() -> anyhow::Result<()> {
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("goop")
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 750.0))
        .build(&event_loop)?;

    let _webview = WebViewBuilder::new()
        .with_url("http://127.0.0.1:8187")
        .with_devtools(true)
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: tao::event::WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

// ── server mode ────────────────────────────────────────────────

async fn run_server() -> anyhow::Result<()> {
    let session = Session::new(256)?;
    server::serve(session).await
}

// ── helpers ────────────────────────────────────────────────────

async fn is_server_running() -> bool {
    tokio::net::TcpStream::connect("127.0.0.1:8187")
        .await
        .is_ok()
}
