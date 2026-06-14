use rig::client::{CompletionClient, ProviderClient};
use rig::completion::Prompt;
use rig::providers::deepseek;
use rig_derive::rig_tool;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

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

                println!("\x1b[2mthinking…\x1b[0m");
                match agent.prompt(trimmed).await {
                    Ok(response) => {
                        println!("\n\x1b[1mresponse:\x1b[0m\n{response}\n");
                    }
                    Err(e) => {
                        eprintln!("\x1b[1;31merror:\x1b[0m {e}\n");
                    }
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
