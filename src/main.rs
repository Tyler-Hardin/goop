use futures::StreamExt;
use rig::client::{CompletionClient, ProviderClient};
use rig::memory::InMemoryConversationMemory;
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use rig_derive::rig_tool;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;

#[rig_tool(
    description = "Replace old_str with new_str in file at path. old_str must be unique.",
    required(command)
)]
async fn replace(
    path: std::path::PathBuf,
    old_str: String,
    new_str: String,
) -> Result<String, rig::tool::ToolError> {
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let count = content.matches(&old_str).count();
    if count == 0 {
        Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::new(std::io::ErrorKind::NotFound, "old_str not found"),
        )))
    } else if count > 1 {
        Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("old_str found {count} times, must be unique"),
            ),
        )))
    } else {
        let new_content = content.replacen(&old_str, &new_str, 1);
        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
        Ok(format!("Replaced 1 occurrence in {}", path.display()))
    }
}

#[rig_tool(
    description = "Run command in shell",
    required(command)
)]
async fn shell(command: String) -> Result<String, rig::tool::ToolError> {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .output()
        .await
        .map(|out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.is_empty() {
                stdout.into_owned()
            } else {
                format!("{stdout}{stderr}")
            }
        })
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))
}

#[rig_tool(
    description = "Write content to file at path",
    required(path, content)
)]
async fn write(
    path: std::path::PathBuf,
    content: String,
) -> Result<String, rig::tool::ToolError> {
    tokio::fs::write(&path, &content)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    Ok(format!("Wrote {} bytes to {}", content.len(), path.display()))
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RST: &str = "\x1b[0m";

async fn print_thinking() {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(format!("{DIM}thinking…{RST}").as_bytes()).await.unwrap();
    stdout.flush().await.unwrap();
}

async fn clear_thinking() {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(b"\r\x1b[K").await.unwrap();
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
        .write_all(
            format!("    {BOLD}{key}:{RST} {GREEN}{value}{RST}\n").as_bytes(),
        )
        .await
        .unwrap();
    stdout.flush().await.unwrap();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = deepseek::Client::from_env()?;

    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        // Read intentionally omitted. shell cat is sufficient and keeping the
        // toolset minimalistic is best.
        .tool(Replace)
        .tool(Shell)
        .tool(Write)
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

                let mut response_text = String::new();
                let mut thinking = true;
                print_thinking().await;

                let mut stream = agent.stream_prompt(&prompt).await;

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::Text(text),
                        )) => {
                            if thinking {
                                clear_thinking().await;
                                thinking = false;
                            }
                            {
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(text.text.as_bytes()).await?;
                                stdout.flush().await?;
                            }
                            response_text.push_str(&text.text);
                        }
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ToolCall { tool_call, .. },
                        )) => {
                            if thinking {
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
                                thinking = false;
                            }
                            let args = tool_call.function.arguments.to_string();
                            tool_header(&tool_call.function.name).await;
                            tool_arg("command", &args).await;
                        }
                        Ok(rig::agent::MultiTurnStreamItem::StreamUserItem(
                            rig::streaming::StreamedUserContent::ToolResult { tool_result, .. },
                        )) => {
                            // Tool result — print if non-empty, then show thinking again
                            let text: String = tool_result
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    rig::message::ToolResultContent::Text(t) => Some(t.text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            if !text.is_empty() {
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(b"\n").await?;
                                stdout.write_all(format!("{DIM}{text}{RST}").as_bytes()).await?;
                                stdout.flush().await?;
                            }
                            // Model is thinking about the next step
                            print_thinking().await;
                            thinking = true;
                        }
                        Ok(rig::agent::MultiTurnStreamItem::FinalResponse(response)) => {
                            if thinking {
                                clear_thinking().await;
                                thinking = false;
                            }
                            // If we haven't accumulated any text yet, use the final response
                            if response_text.is_empty() {
                                let r = response.response();
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(r.as_bytes()).await?;
                                stdout.flush().await?;
                            }
                            {
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
                            }
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if thinking {
                                clear_thinking().await;
                                thinking = false;
                            }
                            let mut stderr = tokio::io::stderr();
                            stderr
                                .write_all(
                                    format!("\x1b[1;31merror:\x1b[0m {e}\n").as_bytes(),
                                )
                                .await?;
                            stderr.flush().await?;
                            break;
                        }
                    }
                }

                // If loop ended without FinalResponse but we have text, add a newline
                if !response_text.is_empty() && thinking {
                    let mut stdout = tokio::io::stdout();
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
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
