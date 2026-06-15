use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;
use wry::WebViewBuilderExtUnix;

use crate::server;
use crate::session::Session;

/// Launch the desktop GUI, auto-starting a server if none is running.
pub fn run(session_name: Option<String>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;

    if rt.block_on(crate::is_server_running()) {
        tracing::info!("found existing server on :8187, opening webview as client");
        run_client()
    } else {
        tracing::info!("no server found, starting primary");
        run_primary(rt, session_name)
    }
}

/// GUI mode when we own the session + server.
fn run_primary(rt: tokio::runtime::Runtime, session_name: Option<String>) -> anyhow::Result<()> {
    let session = rt.block_on(async { Session::new(256, session_name) })?;
    tracing::info!("session · {}", session.name());
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
fn run_client() -> anyhow::Result<()> {
    open_webview()
}

fn open_webview() -> anyhow::Result<()> {
    // GTK must be initialized before wry can embed WebKitGTK.
    // On Linux, wry requires build_gtk(window.default_vbox()) instead of
    // build(&window) — raw window handles silently produce a blank window.
    gtk::init()?;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("goop")
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 750.0))
        .build(&event_loop)?;

    // tao creates a default GtkBox as the window's child.  We add the
    // webview into that box so it fills the window.
    let vbox = window
        .default_vbox()
        .expect("tao window should have a default GtkBox");
    let _webview = WebViewBuilder::new()
        .with_url("http://127.0.0.1:8187")
        .with_devtools(true)
        .build_gtk(vbox)?;

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
