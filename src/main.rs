mod tools;

use futures::StreamExt;
use rig::client::{CompletionClient, ProviderClient};
use rig::memory::InMemoryConversationMemory;
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::sync::Arc;
use streamdown_parser::Parser;
use streamdown_render::Renderer;
use tokio::io::AsyncWriteExt as _;

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RST: &str = "\x1b[0m";

/// Wipe the current line and reset cursor to column 0.
async fn wipe_line() {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\r\x1b[K").await.unwrap();
    stdout.flush().await.unwrap();
}

/// Show a single-line pending preview.  Always terminated by \r\x1b[K so the
/// next render / status line can overwrite it cleanly.
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

const MAX_ARG_LEN: usize = 80;
const MAX_RESULT_LEN: usize = 500;

/// Truncate `s` to at most `max_chars` characters, appending "…" if cut.
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

/// Which phase of the streaming loop we are in.
enum Phase {
    /// "thinking…" is on screen; no markdown emitted yet this turn.
    Thinking,
    /// A markdown pending preview is on the current line (dim).
    Preview,
    /// We just rendered tool info; next token will start a fresh turn.
    AfterTool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = deepseek::Client::from_env()?;

    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .tool(tools::Read)
        .tool(tools::Replace)
        .tool(tools::Shell)
        .tool(tools::Write)
        .max_tokens(100_000)
        .default_max_turns(100)
        .memory(InMemoryConversationMemory::new())
        .conversation_id("default")
        .build();

    let agent = Arc::new(agent);

    let mut rl = DefaultEditor::new()?;

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

    let term_width = streamdown_render::terminal_width();

    loop {
        match rl.readline("\x1b[1;33m»\x1b[0m ") {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                rl.add_history_entry(trimmed)?;

                let agent = agent.clone();
                let prompt = trimmed.to_string();

                // ---- per-turn parser & renderer ----
                let mut parser = Parser::new();
                let mut renderer = Renderer::new(std::io::stdout(), term_width);
                let mut line_buf = String::new();
                let mut phase = Phase::Thinking;

                // Print initial "thinking…" indicator.
                {
                    let mut stdout = tokio::io::stdout();
                    stdout
                        .write_all(format!("{DIM}thinking…{RST}").as_bytes())
                        .await?;
                    stdout.flush().await?;
                }

                let mut stream = agent.stream_prompt(&prompt).await;

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::Text(text),
                        )) => {
                            line_buf.push_str(&text.text);

                            // Process every complete line through parser + renderer.
                            while let Some(pos) = line_buf.find('\n') {
                                let complete = line_buf[..pos].to_string();
                                line_buf = line_buf[pos + 1..].to_string();

                                // Wipe thinking / preview line before first output.
                                if matches!(phase, Phase::Thinking | Phase::Preview) {
                                    wipe_line().await;
                                }

                                let events = parser.parse_line(&complete);
                                for event in &events {
                                    renderer.render_event(event).unwrap();
                                }

                                phase = Phase::Preview;
                            }

                            // Show the incomplete trailing line as a dim preview.
                            if !line_buf.is_empty() {
                                show_pending(&line_buf).await;
                                phase = Phase::Preview;
                            }
                        }
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ToolCall { tool_call, .. },
                        )) => {
                            // Wipe current line, finalize any open parser state.
                            wipe_line().await;
                            let events = parser.finalize();
                            for event in &events {
                                renderer.render_event(event).unwrap();
                            }

                            // Reset for next assistant turn.
                            parser = Parser::new();
                            renderer = Renderer::new(std::io::stdout(), term_width);
                            line_buf.clear();

                            // Display the tool call prettily.
                            let args_str = tool_call.function.arguments.to_string();
                            tool_header(&tool_call.function.name).await;

                            match serde_json::from_str::<serde_json::Value>(&args_str) {
                                Ok(serde_json::Value::Object(obj)) => {
                                    for (key, value) in &obj {
                                        let display_val = match value {
                                            serde_json::Value::String(s) => s.clone(),
                                            other => other.to_string(),
                                        };
                                        tool_arg(key, &ellipsize(&display_val, MAX_ARG_LEN)).await;
                                    }
                                }
                                _ => {
                                    tool_arg("args", &ellipsize(&args_str, MAX_ARG_LEN)).await;
                                }
                            }
                            phase = Phase::AfterTool;
                        }
                        Ok(rig::agent::MultiTurnStreamItem::StreamUserItem(
                            rig::streaming::StreamedUserContent::ToolResult { tool_result, .. },
                        )) => {
                            let text: String = tool_result
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    rig::message::ToolResultContent::Text(t) => {
                                        Some(t.text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            if !text.is_empty() {
                                let displayed = ellipsize(&text, MAX_RESULT_LEN);
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(b"\n").await?;
                                stdout
                                    .write_all(format!("{DIM}{displayed}{RST}").as_bytes())
                                    .await?;
                                stdout.flush().await?;
                            }

                            // Show thinking indicator on a fresh line.
                            let mut stdout = tokio::io::stdout();
                            stdout
                                .write_all(format!("{DIM}thinking…{RST}").as_bytes())
                                .await?;
                            stdout.flush().await?;
                            phase = Phase::Thinking;
                        }
                        Ok(rig::agent::MultiTurnStreamItem::FinalResponse(_response)) => {
                            // Wipe current line.
                            wipe_line().await;

                            // Parse any remaining partial line.
                            if !line_buf.is_empty() {
                                let events = parser.parse_line(&line_buf);
                                for event in &events {
                                    renderer.render_event(event).unwrap();
                                }
                                line_buf.clear();
                            }
                            // Finalize parser.
                            let events = parser.finalize();
                            for event in &events {
                                renderer.render_event(event).unwrap();
                            }

                            // Trailing blank line.
                            let mut stdout = tokio::io::stdout();
                            stdout.write_all(b"\n").await?;
                            stdout.flush().await?;
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            wipe_line().await;
                            let mut stderr = tokio::io::stderr();
                            stderr
                                .write_all(format!("\x1b[1;31merror:\x1b[0m {e}\n").as_bytes())
                                .await?;
                            stderr.flush().await?;
                            break;
                        }
                    }
                }

                // If the loop ended without FinalResponse, flush whatever is left.
                match phase {
                    Phase::Thinking | Phase::Preview => {
                        wipe_line().await;
                        // Parse any remaining partial line.
                        if !line_buf.is_empty() {
                            let events = parser.parse_line(&line_buf);
                            for event in &events {
                                renderer.render_event(event).unwrap();
                            }
                        }
                        // Finalize.
                        let events = parser.finalize();
                        for event in &events {
                            renderer.render_event(event).unwrap();
                        }
                        let mut stdout = tokio::io::stdout();
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                    Phase::AfterTool => {
                        // Tool was the last thing; no markdown to flush.
                    }
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
