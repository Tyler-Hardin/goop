use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::sync::Arc;
use streamdown_parser::Parser;
use streamdown_render::Renderer;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::broadcast;

use crate::events::SessionEvent;
use crate::session::Session;

// ── ANSI constants ──────────────────────────────────────────────

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
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

/// Which phase of rendering we are in (drives wipe / preview behaviour).
enum Phase {
    /// "thinking…" or a dim preview is on screen.
    Thinking,
    /// We are rendering markdown lines.
    Preview,
    /// We just rendered tool info; next token starts a fresh turn.
    AfterTool,
}

// ── low-level terminal ops ──────────────────────────────────────

async fn wipe_line() {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\r\x1b[K").await.unwrap();
    stdout.flush().await.unwrap();
}

async fn show_pending(raw: &str) {
    let first_line = raw.lines().next().unwrap_or("");
    let preview = if first_line.chars().count() > 120 {
        let head: String = first_line.chars().take(117).collect();
        format!("{head}…")
    } else {
        first_line.to_string()
    };
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\r\x1b[K").await.unwrap();
    if !preview.is_empty() {
        stdout
            .write_all(format!("{DIM}{preview}{RST}").as_bytes())
            .await
            .unwrap();
    }
    stdout.flush().await.unwrap();
}

async fn tool_header(name: &str) {
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(
            format!(
                "{DIM}  ────────────────────────────────────────{RST}\n\
                 {BOLD}  ▸ {name}{RST}\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stdout.flush().await.unwrap();
}

async fn tool_arg(key: &str, value: &str) {
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(format!("    {BOLD}{key}:{RST} {GREEN}{value}{RST}\n").as_bytes())
        .await
        .unwrap();
    stdout.flush().await.unwrap();
}

// ── terminal view ───────────────────────────────────────────────

/// Renders session events to the terminal and runs the REPL input loop.
pub struct TerminalView {
    session: Arc<Session>,
}

impl TerminalView {
    pub fn new(session: Arc<Session>) -> Self {
        Self { session }
    }

    /// Run the REPL. Blocks until the user exits.
    pub async fn run(&self) -> anyhow::Result<()> {
        let mut rl = DefaultEditor::new()?;
        let term_width = streamdown_render::terminal_width();

        // Banner.
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

        loop {
            match rl.readline("\x1b[1;33m»\x1b[0m ") {
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    rl.add_history_entry(trimmed)?;

                    // Subscribe *before* submitting so we don't miss events.
                    let mut rx = self.session.subscribe();

                    // Queue the prompt; the background worker picks it up.
                    self.session.submit(trimmed);

                    // ── per-turn render state ──
                    let mut parser = Parser::new();
                    let mut renderer = Renderer::new(std::io::stdout(), term_width);
                    let mut line_buf = String::new();
                    let mut phase = Phase::Thinking;

                    // Initial "thinking…".
                    {
                        let mut stdout = tokio::io::stdout();
                        stdout
                            .write_all(format!("{DIM}thinking…{RST}").as_bytes())
                            .await?;
                        stdout.flush().await?;
                    }

                    // Consume events until FinalResponse or Error.
                    loop {
                        match rx.recv().await {
                            Ok(SessionEvent::UserPrompt { .. }) => {
                                // We just submitted this; nothing to render.
                            }

                            Ok(SessionEvent::AssistantText(text)) => {
                                line_buf.push_str(&text);

                                while let Some(pos) = line_buf.find('\n') {
                                    let complete = line_buf[..pos].to_string();
                                    line_buf = line_buf[pos + 1..].to_string();

                                    if matches!(phase, Phase::Thinking | Phase::Preview) {
                                        wipe_line().await;
                                    }

                                    let events = parser.parse_line(&complete);
                                    for event in &events {
                                        renderer.render_event(event).unwrap();
                                    }

                                    phase = Phase::Preview;
                                }

                                if !line_buf.is_empty() {
                                    show_pending(&line_buf).await;
                                    phase = Phase::Preview;
                                }
                            }

                            Ok(SessionEvent::ToolCall { name, arguments }) => {
                                wipe_line().await;
                                let events = parser.finalize();
                                for event in &events {
                                    renderer.render_event(event).unwrap();
                                }

                                parser = Parser::new();
                                renderer = Renderer::new(std::io::stdout(), term_width);
                                line_buf.clear();

                                tool_header(&name).await;

                                match arguments {
                                    serde_json::Value::Object(obj) => {
                                        for (key, value) in &obj {
                                            let display_val = match value {
                                                serde_json::Value::String(s) => s.clone(),
                                                other => other.to_string(),
                                            };
                                            tool_arg(key, &ellipsize(&display_val, MAX_ARG_LEN))
                                                .await;
                                        }
                                    }
                                    other => {
                                        tool_arg(
                                            "args",
                                            &ellipsize(&other.to_string(), MAX_ARG_LEN),
                                        )
                                        .await;
                                    }
                                }

                                phase = Phase::AfterTool;
                            }

                            Ok(SessionEvent::ToolResult { content }) => {
                                if !content.is_empty() {
                                    let displayed = ellipsize(&content, MAX_RESULT_LEN);
                                    let mut stdout = tokio::io::stdout();
                                    stdout.write_all(b"\n").await?;
                                    stdout
                                        .write_all(format!("{DIM}{displayed}{RST}").as_bytes())
                                        .await?;
                                    stdout.flush().await?;
                                }

                                let mut stdout = tokio::io::stdout();
                                stdout
                                    .write_all(format!("{DIM}thinking…{RST}").as_bytes())
                                    .await?;
                                stdout.flush().await?;
                                phase = Phase::Thinking;
                            }

                            Ok(SessionEvent::Thinking) => {
                                // Already showing thinking indicator.
                            }

                            Ok(SessionEvent::FinalResponse) => {
                                wipe_line().await;

                                if !line_buf.is_empty() {
                                    let events = parser.parse_line(&line_buf);
                                    for event in &events {
                                        renderer.render_event(event).unwrap();
                                    }
                                    line_buf.clear();
                                }
                                let events = parser.finalize();
                                for event in &events {
                                    renderer.render_event(event).unwrap();
                                }

                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
                                break;
                            }

                            Ok(SessionEvent::Error(e)) => {
                                wipe_line().await;
                                let mut stderr = tokio::io::stderr();
                                stderr
                                    .write_all(format!("\x1b[1;31merror:\x1b[0m {e}\n").as_bytes())
                                    .await?;
                                stderr.flush().await?;
                                break;
                            }

                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                let mut stderr = tokio::io::stderr();
                                stderr
                                    .write_all(
                                        format!(
                                            "\x1b[1;33mwarning: view lagged by {n} events\x1b[0m\n"
                                        )
                                        .as_bytes(),
                                    )
                                    .await?;
                                stderr.flush().await?;
                            }

                            Err(broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }

                    // Flush remaining state if the stream ended without FinalResponse.
                    match phase {
                        Phase::Thinking | Phase::Preview => {
                            wipe_line().await;
                            if !line_buf.is_empty() {
                                let events = parser.parse_line(&line_buf);
                                for event in &events {
                                    renderer.render_event(event).unwrap();
                                }
                            }
                            let events = parser.finalize();
                            for event in &events {
                                renderer.render_event(event).unwrap();
                            }
                            let mut stdout = tokio::io::stdout();
                            stdout.write_all(b"\n").await?;
                            stdout.flush().await?;
                        }
                        Phase::AfterTool => {}
                    }
                }

                Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                    let mut stdout = tokio::io::stdout();
                    stdout.write_all(b"\x1b[2mbye.\x1b[0m\n").await?;
                    stdout.flush().await?;
                    break;
                }

                Err(e) => {
                    let mut stderr = tokio::io::stderr();
                    stderr
                        .write_all(format!("readline error: {e}\n").as_bytes())
                        .await?;
                    stderr.flush().await?;
                    break;
                }
            }
        }

        Ok(())
    }
}
