mod events;
mod server;
mod session;
mod terminal;
mod tools;

use clap::Parser;
use session::Session;
use terminal::TerminalView;

#[derive(Parser)]
#[command(name = "goop")]
struct Args {
    /// Launch the desktop GUI (native webview) instead of the terminal REPL.
    #[arg(long)]
    gui: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rig_core=warn")),
        )
        .init();

    if args.gui {
        run_gui()
    } else {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run_terminal())
    }
}

async fn run_terminal() -> anyhow::Result<()> {
    let session = Session::new(256)?;

    // Run terminal REPL and web server concurrently.
    let view = TerminalView::new(session.clone());
    let term = tokio::spawn(async move { view.run().await });
    let web = tokio::spawn(server::serve(session));

    tokio::select! {
        r = term => { r??; }
        r = web => { r??; }
    }

    Ok(())
}

fn run_gui() -> anyhow::Result<()> {
    // We need the main thread free for the wry/tao event loop, so we
    // create a separate tokio runtime for the backend (session + server).
    let rt = tokio::runtime::Runtime::new()?;

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
        // Notify the main thread that axum::serve has started.
        let _ = ready_tx.send(());
        tracing::info!("web server on http://127.0.0.1:8187");
        axum::serve(listener, app).await.unwrap();
    });

    // Wait until the server task has actually started.
    let _ = ready_rx.recv();

    // Open the native webview window.
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("goop")
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 750.0))
        .build(&event_loop)?;

    // Load via the embedded HTTP server so the page gets a proper
    // origin — with_html produces a null origin that blocks CDN
    // scripts and may prevent WebSocket connections in some webviews.
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
