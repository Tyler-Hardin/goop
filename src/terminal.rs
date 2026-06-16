use futures::{SinkExt, StreamExt};
use rustyline::Cmd;
use rustyline::DefaultEditor;
use rustyline::KeyEvent;
use rustyline::error::ReadlineError;
use std::io::BufRead;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::events::SessionEvent;
use crate::memory::prompt_history_path;
use streamdown_parser::Parser;
use streamdown_render::Renderer;

// ── ANSI constants ──────────────────────────────────────────────

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const RST: &str = "\x1b[0m";

const MAX_ARG_LEN: usize = 80;
const MAX_RESULT_LEN: usize = 500;

// ── helpers ─────────────────────────────────────────────────────

fn ellipsize(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else if max_chars <= 1 {
        "…".to_string()
    } else {
        let head: String = s.chars().take(max_chars - 1).collect();
        format!("{head}…")
    }
}

// ── PrinterWriter: adapts ExternalPrinter to std::io::Write ────

/// Buffers rendered output and prints each complete line through
/// rustyline's external printer.  When there is no readline prompt
/// active, `ExternalPrinter` automatically falls back to direct
/// stdout writes.
struct PrinterWriter<P: rustyline::ExternalPrinter> {
    printer: Arc<StdMutex<P>>,
    buf: String,
}

impl<P: rustyline::ExternalPrinter> PrinterWriter<P> {
    fn flush_buf(&mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.printer
                .lock()
                .expect("printer mutex poisoned")
                .print(line)
                .ok();
        }
    }
}

impl<P: rustyline::ExternalPrinter> std::io::Write for PrinterWriter<P> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let s = std::str::from_utf8(data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.buf.push_str(s);
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..=pos].to_string();
            self.buf = self.buf[pos + 1..].to_string();
            self.printer
                .lock()
                .expect("printer mutex poisoned")
                .print(line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_buf();
        Ok(())
    }
}

// ── terminal client (connects to server via WS) ─────────────────

pub struct TerminalClient;

impl TerminalClient {
    /// Connect to a running goop server and start the terminal REPL.
    pub async fn run(session_name: &str) -> anyhow::Result<()> {
        // ── connect to server ────────────────────────────────
        let url = format!("ws://127.0.0.1:8187/ws?session={session_name}");
        let (ws_stream, _) = connect_async(&url).await?;
        let (ws_tx, ws_rx) = ws_stream.split();
        let ws_tx = Arc::new(tokio::sync::Mutex::new(ws_tx));

        // ── prompt history ───────────────────────────────────
        // Load the global prompt history (JSONL, one JSON string per line).
        // The server writes every prompt from every client here, so the
        // terminal always sees the complete history.
        let history_path = prompt_history_path();
        if let Some(parent) = history_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut rl = DefaultEditor::new()?;
        sync_history_from_file(&mut rl, &history_path);
        // Ctrl+J inserts a literal newline for multiline input.
        rl.bind_sequence(KeyEvent::ctrl('j'), Cmd::Insert(1, "\n".into()));
        let term_width = streamdown_render::terminal_width();

        let printer = Arc::new(StdMutex::new(rl.create_external_printer()?));

        // ── channel plumbing ──────────────────────────────────
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Option<String>>();
        let (ready_tx, ready_rx) = mpsc::unbounded_channel::<()>();
        let ready_rx = Arc::new(StdMutex::new(Some(ready_rx)));

        // Events from WS → render task.
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();

        // Shared session name — captured from SessionInfo event, printed on exit.
        let session_name: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));

        // Render task signals main loop when output is fully rendered.
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();

        // ── banner ────────────────────────────────────────────
        {
            let mut stdout = tokio::io::stdout();
            stdout
                .write_all(
                    "\x1b[1;36m╔════════════════════════════════╗\n\
                     ║   goop — ai agent repl         ║\n\
                     ╚════════════════════════════════╝\x1b[0m\n"
                        .as_bytes(),
                )
                .await?;
            stdout.flush().await?;
        }

        // ── permanent readline thread ──────────────────────────
        let thread_ready = ready_rx.clone();
        std::thread::spawn(move || {
            'outer: loop {
                let mut buffer = String::new();
                let mut first = true;

                loop {
                    let prompt = if first {
                        "\x1b[1;33m»\x1b[0m "
                    } else {
                        "  \x1b[2m…\x1b[0m "
                    };

                    match rl.readline(prompt) {
                        Ok(line) => {
                            if first && line.trim().is_empty() {
                                continue 'outer;
                            }
                            first = false;

                            if line.ends_with('\\') {
                                let (stripped, _) = line.split_at(line.len() - 1);
                                buffer.push_str(stripped);
                                buffer.push('\n');
                            } else {
                                buffer.push_str(&line);
                                let trimmed = buffer.trim().to_string();
                                if !trimmed.is_empty() {
                                    rl.add_history_entry(&trimmed).ok();
                                    input_tx.send(Some(trimmed)).ok();
                                }
                                if let Some(rx) =
                                    thread_ready.lock().expect("ready mutex poisoned").as_mut()
                                {
                                    if rx.blocking_recv().is_none() {
                                        break 'outer;
                                    }
                                } else {
                                    break 'outer;
                                }
                                // Reload history from disk so prompts from
                                // web/GUI clients show up in up/down nav.
                                sync_history_from_file(&mut rl, &history_path);
                                break;
                            }
                        }
                        Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                            input_tx.send(None).ok();
                            break 'outer;
                        }
                        Err(_) => {
                            input_tx.send(None).ok();
                            break 'outer;
                        }
                    }
                }
            }
        });

        // ── single render task ────────────────────────────────
        let render_printer = printer.clone();
        let render_handle = tokio::spawn(async move {
            render_loop(render_printer, ev_rx, done_tx, term_width).await;
        });

        // ── WS receive task ───────────────────────────────────
        // Reads JSON SessionEvent from the WebSocket and forwards
        // to the render task. Filters out UserPrompt echoes — the
        // terminal user already saw their input on the readline.
        // Captures SessionInfo for display on exit.
        let fwd_tx = ev_tx.clone();
        let mut ws_rx = ws_rx;
        let ws_session_name = session_name.clone();
        let fwd_handle = tokio::spawn(async move {
            while let Some(msg) = ws_rx.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        let event: SessionEvent = match serde_json::from_str(&text) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        // Capture session name when we see SessionInfo.
                        if let SessionEvent::SessionInfo { ref name } = event {
                            *ws_session_name.lock().expect("session name mutex poisoned") =
                                Some(name.clone());
                        }
                        // Suppress UserPrompt echoes — the terminal
                        // user already saw their input on the readline.
                        if matches!(event, SessionEvent::UserPrompt { .. }) {
                            continue;
                        }
                        if fwd_tx.send(event).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        tracing::error!("ws recv error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        });

        // ── main loop ──────────────────────────────────────────
        loop {
            let raw = tokio::select! {
                r = input_rx.recv() => r,
                _ = tokio::signal::ctrl_c() => {
                    None
                }
            };

            match raw {
                Some(Some(line)) => {
                    // Send prompt over WebSocket.
                    let payload =
                        serde_json::json!({"type": "prompt", "content": &line}).to_string();
                    {
                        let mut tx = ws_tx.lock().await;
                        if tx.send(Message::Text(payload.into())).await.is_err() {
                            let name = session_name
                                .lock()
                                .expect("session name mutex poisoned")
                                .clone();
                            print_exit_banner(&name).await;
                            break;
                        }
                    }

                    let mut cancelled = false;

                    // Wait for render flush, handling Ctrl+C for cancel.
                    loop {
                        tokio::select! {
                            _ = done_rx.recv() => {
                                break; // session finished
                            }
                            _ = tokio::signal::ctrl_c() => {
                                if cancelled {
                                    // Second Ctrl+C → exit.
                                    let name = session_name.lock().expect("session name mutex poisoned").clone();
                                    print_exit_banner(&name).await;
                                    *ready_rx.lock().expect("ready mutex poisoned") = None;
                                    drop(ev_tx);
                                    render_handle.abort();
                                    fwd_handle.abort();
                                    return Ok(());
                                }
                                // First Ctrl+C: send cancel over WS.
                                let cancel_msg =
                                    serde_json::json!({"type": "cancel"}).to_string();
                                let mut tx = ws_tx.lock().await;
                                let _ = tx.send(Message::Text(cancel_msg.into())).await;
                                cancelled = true;
                            }
                        }
                    }

                    ready_tx.send(()).ok();
                }
                _ => {
                    let name = session_name
                        .lock()
                        .expect("session name mutex poisoned")
                        .clone();
                    print_exit_banner(&name).await;
                    *ready_rx.lock().expect("ready mutex poisoned") = None;
                    break;
                }
            }
        }

        // Clean shutdown.
        drop(ev_tx);
        render_handle.abort();
        fwd_handle.abort();

        Ok(())
    }
}

// ── unified render loop ─────────────────────────────────────────
//
// This is the single rendering pipeline.  It receives session
// events from an mpsc receiver, drives a streamdown Parser +
// Renderer, and prints every line through the external printer.
//
// When the LLM finishes a turn (FinalResponse / Error) it sends
// `()` on `done_tx` so the main loop knows the output is fully
// visible and it's safe to show the next readline prompt.

/// Owns all mutable rendering state so the event loop can call
/// methods instead of macros.
struct RenderState<P: rustyline::ExternalPrinter> {
    printer: Arc<StdMutex<P>>,
    term_width: usize,
    parser: Parser,
    renderer: Option<Renderer<PrinterWriter<P>>>,
    line_buf: String,
    in_turn: bool,
}

impl<P: rustyline::ExternalPrinter> RenderState<P> {
    fn new(printer: Arc<StdMutex<P>>, term_width: usize) -> Self {
        let renderer = Some(Renderer::new(
            PrinterWriter {
                printer: printer.clone(),
                buf: String::new(),
            },
            term_width,
        ));
        Self {
            printer,
            term_width,
            parser: Parser::new(),
            renderer,
            line_buf: String::new(),
            in_turn: false,
        }
    }

    /// Lock the printer mutex for direct (non-markdown) output.
    fn lock_printer(&self) -> std::sync::MutexGuard<'_, P> {
        self.printer.lock().expect("printer mutex poisoned")
    }

    /// Get a mutable reference to the renderer.
    /// Always `Some` — the renderer is only dropped transiently
    /// during `reset_renderer`.
    fn renderer_mut(&mut self) -> &mut Renderer<PrinterWriter<P>> {
        self.renderer.as_mut().expect("renderer unexpectedly None")
    }

    /// Drop the renderer (which also drops its inner writer),
    /// then create a fresh writer + renderer pair.
    fn reset_renderer(&mut self) {
        drop(self.renderer.take());
        self.renderer = Some(Renderer::new(
            PrinterWriter {
                printer: self.printer.clone(),
                buf: String::new(),
            },
            self.term_width,
        ));
    }

    /// Flush markdown: parse any buffered partial line, then
    /// finalize the parser and render events.
    fn flush_markdown(&mut self) {
        if !self.line_buf.is_empty() {
            let events = self.parser.parse_line(&self.line_buf);
            let r = self.renderer_mut();
            for evt in &events {
                r.render_event(evt).expect("markdown render_event failed");
            }
            self.line_buf.clear();
        }
        let events = self.parser.finalize();
        let r = self.renderer_mut();
        for evt in &events {
            r.render_event(evt)
                .expect("markdown finalize render_event failed");
        }
    }
}

pub(crate) async fn render_loop<P: rustyline::ExternalPrinter>(
    printer: Arc<StdMutex<P>>,
    mut events: mpsc::UnboundedReceiver<SessionEvent>,
    done_tx: mpsc::UnboundedSender<()>,
    term_width: usize,
) {
    use crate::events::PromptSource;

    let mut state = RenderState::new(printer, term_width);

    while let Some(event) = events.recv().await {
        match event {
            SessionEvent::SessionInfo { ref name } => {
                state
                    .lock_printer()
                    .print(format!("{DIM}  ● session {name}{RST}\n"))
                    .ok();
            }

            // Web/GUI clients use this to show/hide a Cancel button;
            // the terminal tracks its own in-turn state.
            SessionEvent::SessionState { .. } => {}

            SessionEvent::UserPrompt {
                ref content,
                ref source,
            } => {
                // Don't echo back what the user just typed in
                // the terminal — they already saw it.
                if *source == PromptSource::Terminal {
                    state.in_turn = true;
                    continue;
                }

                // Finish previous turn.
                if state.in_turn {
                    state.flush_markdown();
                    state.reset_renderer();
                }

                // Start new turn.
                state.parser = Parser::new();
                state.line_buf.clear();
                state.in_turn = true;

                let prompt = ellipsize(content, 80);
                state
                    .lock_printer()
                    .print(format!("{BOLD}{CYAN}»{RST} {prompt}\n"))
                    .ok();
            }

            SessionEvent::AssistantText(text) => {
                state.line_buf.push_str(&text);
                while let Some(pos) = state.line_buf.find('\n') {
                    let complete = state.line_buf[..pos].to_string();
                    state.line_buf = state.line_buf[pos + 1..].to_string();
                    let events = state.parser.parse_line(&complete);
                    let r = state.renderer_mut();
                    for evt in &events {
                        r.render_event(evt).expect("markdown render_event failed");
                    }
                }
            }

            SessionEvent::ToolCall {
                ref name,
                ref arguments,
            } => {
                state.flush_markdown();
                state.reset_renderer();
                state.parser = Parser::new();
                state.line_buf.clear();

                state
                    .lock_printer()
                    .print(format!(
                        "{DIM}  ────────────────────────────────────────{RST}\n\
                         {BOLD}  ▸ {name}{RST}\n"
                    ))
                    .ok();

                match arguments {
                    serde_json::Value::Object(obj) => {
                        for (key, value) in obj {
                            let display_val = match value {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            state
                                .lock_printer()
                                .print(format!(
                                    "    {BOLD}{key}:{RST} {GREEN}{}{RST}\n",
                                    ellipsize(&display_val, MAX_ARG_LEN)
                                ))
                                .ok();
                        }
                    }
                    other => {
                        state
                            .lock_printer()
                            .print(format!(
                                "    {BOLD}args:{RST} {GREEN}{}{RST}\n",
                                ellipsize(&other.to_string(), MAX_ARG_LEN)
                            ))
                            .ok();
                    }
                }
            }

            SessionEvent::ToolResult { ref content } => {
                if !content.is_empty() {
                    let displayed = ellipsize(content, MAX_RESULT_LEN);
                    state
                        .lock_printer()
                        .print(format!("{DIM}{displayed}{RST}\n"))
                        .ok();
                }
            }

            SessionEvent::Thinking => { /* implicit */ }

            SessionEvent::FinalResponse => {
                state.flush_markdown();
                state.reset_renderer();
                state.parser = Parser::new();
                state.line_buf.clear();
                state.in_turn = false;
                done_tx.send(()).ok();
            }

            SessionEvent::Cancelled => {
                // Flush any partial markdown, then signal done.
                state.flush_markdown();
                state.reset_renderer();
                state.parser = Parser::new();
                state.line_buf.clear();
                state
                    .lock_printer()
                    .print(format!("{DIM}cancelled.{RST}\n"))
                    .ok();
                state.in_turn = false;
                done_tx.send(()).ok();
            }

            SessionEvent::Error(e) => {
                state.line_buf.clear();
                state
                    .lock_printer()
                    .print(format!("\x1b[1;31merror:\x1b[0m {e}\n"))
                    .ok();
                state.in_turn = false;
                done_tx.send(()).ok();
            }
        }
    }
}

/// Clear rustyline's in-memory history and reload from the JSONL file.
fn sync_history_from_file(rl: &mut DefaultEditor, path: &std::path::Path) {
    let _ = rl.clear_history();
    if let Ok(file) = std::fs::File::open(path) {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(prompt) = serde_json::from_str::<String>(&line) {
                rl.add_history_entry(&prompt).ok();
            }
        }
    }
}

/// Print the session-closed banner with the session name for easy copy/paste.
async fn print_exit_banner(name: &Option<String>) {
    let mut stdout = tokio::io::stdout();
    let display = name.as_deref().unwrap_or("unknown");
    let _ = stdout
        .write_all(format!("\x1b[2m  ● session closed · {display}\x1b[0m\n").as_bytes())
        .await;
    let _ = stdout.flush().await;
}
