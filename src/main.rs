mod events;
mod server;
mod session;
mod terminal;
mod tools;

use session::Session;
use terminal::TerminalView;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rig_core=warn")),
        )
        .init();

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
