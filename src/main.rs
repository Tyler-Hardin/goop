mod events;
mod server;
mod session;
mod terminal;
mod tools;

use session::Session;
use terminal::TerminalView;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
