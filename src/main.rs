mod tools;

use futures::StreamExt;
use mdstream::{BlockKind, MdStream, Options};
use rig::client::{CompletionClient, ProviderClient};
use rig::memory::InMemoryConversationMemory;
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const BLUE_BG: &str = "\x1b[44m";
const RST: &str = "\x1b[0m";

/// Wipe the current line and reset cursor to column 0.
async fn wipe_line() {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\r\x1b[K").await.unwrap();
    stdout.flush().await.unwrap();
}

/// Render a single committed markdown block to stdout.
async fn render_block(block: &mdstream::Block) {
    let mut stdout = tokio::io::stdout();
    let text = block.display_or_raw();

    match block.kind {
        BlockKind::Heading => {
            stdout
                .write_all(format!("\n{BOLD}{CYAN}{text}{RST}").as_bytes())
                .await
                .unwrap();
        }
        BlockKind::CodeFence => {
            let mut lines = text.lines();
            if let Some(header) = lines.next() {
                stdout
                    .write_all(format!("{DIM}{header}{RST}\n").as_bytes())
                    .await
                    .unwrap();
            }
            for line in lines {
                stdout
                    .write_all(format!("{BLUE_BG}{DIM} {line}{RST}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
        BlockKind::BlockQuote => {
            for line in text.lines() {
                stdout
                    .write_all(format!("{DIM}  {line}{RST}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
        BlockKind::ThematicBreak => {
            stdout
                .write_all(format!("{DIM}───{RST}\n").as_bytes())
                .await
                .unwrap();
        }
        BlockKind::List => {
            for line in text.lines() {
                stdout
                    .write_all(format!("  {line}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
        BlockKind::Table => {
            for line in text.lines() {
                if line.contains("──") || line.contains("| -") || line.starts_with("|-") {
                    stdout
                        .write_all(format!("{DIM}{line}{RST}\n").as_bytes())
                        .await
                        .unwrap();
                } else {
                    stdout
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
        }
        _ => {
            // Paragraph, HtmlBlock, MathBlock, FootnoteDefinition, Unknown.
            // The raw text typically ends with its own newline(s); don't add extra.
            stdout.write_all(text.as_bytes()).await.unwrap();
            if !text.ends_with('\n') {
                stdout.write_all(b"\n").await.unwrap();
            }
        }
    }
    stdout.flush().await.unwrap();
}

/// Show a single-line pending preview. Always terminated by \r\x1b[K so the
/// next render_block / status line can overwrite it cleanly.
async fn show_pending(raw: &str) {
    let first_line = raw.lines().next().unwrap_or("");
    let preview = if first_line.len() > 120 {
        format!("{}…", &first_line[..117])
    } else {
        first_line.to_string()
    };
    let mut stdout = tokio::io::stdout();
    // Wipe whatever was on this line (old pending / "thinking…") then print.
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

/// Which phase of the streaming loop we are in.
enum Phase {
    /// "thinking…" is on screen; no markdown blocks emitted yet this turn.
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

                let mut md_stream = MdStream::new(Options::default());
                let mut phase = Phase::Thinking;

                // Print initial "thinking…" indicator.
                let mut stdout = tokio::io::stdout();
                stdout
                    .write_all(format!("{DIM}thinking…{RST}").as_bytes())
                    .await?;
                stdout.flush().await?;

                let mut stream = agent.stream_prompt(&prompt).await;

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::Text(text),
                        )) => {
                            let update = md_stream.append(&text.text);

                            // Render any newly-committed blocks.
                            if !update.committed.is_empty() {
                                // Wipe the current line (old pending preview or "thinking…").
                                wipe_line().await;
                                for block in &update.committed {
                                    render_block(block).await;
                                }
                            }

                            // Show the new pending block (single-line dim preview).
                            if let Some(ref pending) = update.pending {
                                show_pending(pending.display_or_raw()).await;
                                phase = Phase::Preview;
                            }
                        }
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ToolCall { tool_call, .. },
                        )) => {
                            // Wipe current line then finalize any pending markdown.
                            wipe_line().await;
                            let final_update = md_stream.finalize();
                            for block in &final_update.committed {
                                render_block(block).await;
                            }

                            // Reset markdown parser.
                            md_stream = MdStream::new(Options::default());

                            let args = tool_call.function.arguments.to_string();
                            tool_header(&tool_call.function.name).await;
                            tool_arg("command", &args).await;
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
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(b"\n").await?;
                                stdout
                                    .write_all(format!("{DIM}{text}{RST}").as_bytes())
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
                            // Wipe current line, finalize, render, done.
                            wipe_line().await;
                            let final_update = md_stream.finalize();
                            for block in &final_update.committed {
                                render_block(block).await;
                            }
                            if let Some(ref pending) = final_update.pending {
                                render_block(pending).await;
                            }
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
                // Only do this if we're still in Thinking or Preview phase (not error/tool).
                match phase {
                    Phase::Thinking | Phase::Preview => {
                        wipe_line().await;
                        let final_update = md_stream.finalize();
                        for block in &final_update.committed {
                            render_block(block).await;
                        }
                        if let Some(ref pending) = final_update.pending {
                            render_block(pending).await;
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
