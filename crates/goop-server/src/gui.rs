use std::io::Write;
use std::sync::Arc;

use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;
use wry::WebViewBuilderExtUnix;

use crate::server;
use crate::session::SessionManager;

/// Embedded icon — a green "goop splat"
static ICON_PNG: &[u8] = include_bytes!("../assets/goop_icon.png");

/// Launch the desktop GUI, auto-starting a server if none is running.
///
/// - **Primary** (no server running): creates a new auto-named session
///   by default so the user can start typing immediately.
/// - **Secondary** (server already running): opens the latest session
///   (by name) so the user resumes where they left off.
/// - If `--session <name>` is given, that name is always used verbatim.
pub fn run(session_name: Option<String>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;

    if rt.block_on(crate::is_server_running()) {
        tracing::info!("found existing server on :8187, opening webview as client");
        // Secondary: if no --session, pick the latest existing session.
        let name = session_name.or_else(fetch_latest_session);
        open_webview(name)
    } else {
        tracing::info!("no server found, starting primary");
        // Primary: if no --session, create a fresh session.
        let name = session_name.unwrap_or_else(crate::session::next_session_name);
        run_primary(rt, Some(name))
    }
}

/// Query the running server for its session list and return the
/// most-recently named session (date-based names sort naturally).
fn fetch_latest_session() -> Option<String> {
    let resp = reqwest::blocking::get("http://127.0.0.1:8187/api/sessions").ok()?;
    let names: Vec<String> = resp.json().ok()?;
    names.into_iter().last()
}

/// GUI mode when we own the session + server.
fn run_primary(rt: tokio::runtime::Runtime, session_name: Option<String>) -> anyhow::Result<()> {
    let config = crate::config::load_config(None, None)?;
    let push_manager = Arc::new(crate::push::PushManager::new());
    let manager = Arc::new(SessionManager::new(config, Arc::clone(&push_manager)));
    rt.block_on(async {
        manager.init_global_mcp().await;
        manager.discover().await
    })?;

    // If the user asked for a specific session, ensure it's loaded.
    if let Some(ref name) = session_name {
        let session = rt.block_on(async { manager.get_or_create(name.clone(), None).await })?;
        tracing::info!("session · {}", session.name());
    }

    let app = server::build_router(manager, push_manager);

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
        axum::serve(listener, app)
            .with_graceful_shutdown(server::shutdown_signal())
            .await
            .expect("server exited unexpectedly");
        // If restart was requested, spawn the new binary.  The GUI
        // webview will reconnect automatically.
        if server::is_restart_requested() {
            server::spawn_new_binary();
        }
    });

    let _ = ready_rx.recv();

    open_webview(session_name)
}

fn open_webview(session_name: Option<String>) -> anyhow::Result<()> {
    // GTK must be initialized before wry can embed WebKitGTK.
    // On Linux, wry requires build_gtk(window.default_vbox()) instead of
    // build(&window) — raw window handles silently produce a blank window.
    gtk::init()?;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("goop")
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 750.0))
        .build(&event_loop)?;

    // Set the window icon so it shows up in the DE window picker /
    // Alt-Tab switcher.  We write the embedded PNG to a temp file
    // and use GTK's set_icon_from_file.
    {
        let icon_path = std::env::temp_dir().join("goop_icon.png");
        if let Err(e) = std::fs::File::create(&icon_path).and_then(|mut f| f.write_all(ICON_PNG)) {
            tracing::warn!("could not write icon file: {e}");
        } else {
            use gtk::prelude::GtkWindowExt;
            let gtk_win = window.gtk_window();
            if let Err(e) = gtk_win.set_icon_from_file(&icon_path) {
                tracing::warn!("failed to set window icon: {e}");
            }
            // Best-effort cleanup; the file is tiny and in /tmp anyway.
            let _ = std::fs::remove_file(&icon_path);
        }
    }

    // tao creates a default GtkBox as the window's child.  We add the
    // webview into that box so it fills the window.
    let vbox = window
        .default_vbox()
        .expect("tao window should have a default GtkBox");

    // If a session was requested, include it in the URL hash so the
    // web UI can pre-select it.
    let url = if let Some(name) = session_name {
        format!("http://127.0.0.1:8187#session={name}")
    } else {
        String::from("http://127.0.0.1:8187")
    };

    let webview = WebViewBuilder::new()
        .with_url(&url)
        .with_devtools(true)
        .build_gtk(vbox)?;

    let mut webview = Some(webview);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: tao::event::WindowEvent::CloseRequested,
            ..
        } = event
        {
            // Destroy the webview before the event loop exits so GTK
            // and WebKit can tear down cleanly — otherwise the implicit
            // drop at closure destruction races with the event loop
            // unwind and can corrupt the C heap.
            drop(webview.take());
            *control_flow = ControlFlow::Exit;
        }
    });
}
