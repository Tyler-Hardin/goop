use futures::StreamExt;
use rig::client::{CompletionClient, ProviderClient};
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use rig_derive::rig_tool;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::io::Write as _;
use std::sync::Arc;

#[rig_tool(
    description = "Replace old_str with new_str in file at path. old_str must be unique.",
    required(command)
)]
async fn replace(
    path: std::path::PathBuf,
    old_str: String,
    new_str: String,
) -> Result<String, rig::tool::ToolError> {
    let content = std::fs::read_to_string(&path)
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
        std::fs::write(&path, &new_content)
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
        Ok(format!("Replaced 1 occurrence in {}", path.display()))
    }
}

#[rig_tool(
    description = "Run command in shell",
    required(command)
)]
async fn shell(command: String) -> Result<String, rig::tool::ToolError> {
    subprocess::Exec::shell(&command)
        .capture()
        .map(|out| {
            let stdout = out.stdout_str();
            let stderr = out.stderr_str();
            if stderr.is_empty() {
                stdout
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
    std::fs::write(&path, &content)
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    Ok(format!("Wrote {} bytes to {}", content.len(), path.display()))
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RST: &str = "\x1b[0m";

fn print_thinking() {
    print!("{DIM}thinking…{RST}");
    let _ = std::io::stdout().flush();
}

fn clear_thinking() {
    print!("\r\x1b[K");
}

fn tool_header(name: &str) {
    println!(
        "{DIM}  ────────────────────────────────────────{RST}\n\
         {BOLD}  ▸ {name}{RST}"
    );
}

fn tool_arg(key: &str, value: &str) {
    println!("    {BOLD}{key}:{RST} {GREEN}{value}{RST}");
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
        .build();

    let agent = Arc::new(agent);

    let mut rl = DefaultEditor::new()?;

    println!(
        "\x1b[1;36m╔════════════════════════════════╗\n\
         ║   goop — ai agent repl         ║\n\
         ╚════════════════════════════════╝\x1b[0m"
    );

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
                print_thinking();

                let mut stream = agent.stream_prompt(&prompt).await;

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::Text(text),
                        )) => {
                            if thinking {
                                clear_thinking();
                                thinking = false;
                            }
                            print!("{text}");
                            let _ = std::io::stdout().flush();
                            response_text.push_str(&text.text);
                        }
                        Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ToolCall { tool_call, .. },
                        )) => {
                            if thinking {
                                println!(); // finish "thinking…" line
                                thinking = false;
                            }
                            let args = tool_call.function.arguments.to_string();
                            tool_header(&tool_call.function.name);
                            tool_arg("command", &args);
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
                                println!();
                                println!("{DIM}{text}{RST}");
                            }
                            // Model is thinking about the next step
                            print_thinking();
                            thinking = true;
                        }
                        Ok(rig::agent::MultiTurnStreamItem::FinalResponse(response)) => {
                            if thinking {
                                clear_thinking();
                                thinking = false;
                            }
                            // If we haven't accumulated any text yet, use the final response
                            if response_text.is_empty() {
                                let r = response.response();
                                print!("{r}");
                                let _ = std::io::stdout().flush();
                            }
                            println!();
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if thinking {
                                clear_thinking();
                                thinking = false;
                            }
                            eprintln!("\x1b[1;31merror:\x1b[0m {e}");
                            break;
                        }
                    }
                }

                // If loop ended without FinalResponse but we have text, add a newline
                if !response_text.is_empty() && thinking {
                    println!();
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                println!("\x1b[2mbye.\x1b[0m");
                break;
            }
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }

    Ok(())
}
