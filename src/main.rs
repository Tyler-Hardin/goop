use rig::client::{CompletionClient, ProviderClient};
use rig::completion::Prompt;
use rig::providers::deepseek;
use rig_derive::rig_tool;

#[rig_tool(
    description = "Replace old_str with new_str in file at path. old_str must be unique.",
    required(command)
)]
async fn replace(path: std::path::PathBuf, old_str: String, new_str: String) -> Result<(), rig::tool::ToolError> {
    let content = std::fs::read_to_string(&path)
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;

    // Count occurances of old_str in content.
    let count = content.matches(&old_str).count();

    if count == 0 {
        Err(rig::tool::ToolError::ToolCallError(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "old_str not found",
        ))))
    }
    else if count > 1 {
        Err(rig::tool::ToolError::ToolCallError(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "old_str found more than once",
        ))))
    } else {
        let new_content = content.replace(&old_str, &new_str);
        std::fs::write(&path, new_content)
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
        Ok(())
    }
} 

#[rig_tool(
    description = "Run command in shell",
    required(command)
)]
async fn shell(command: String) -> Result<String, rig::tool::ToolError> {
    println!("Running command: {}", command);
    subprocess::Exec::shell(command).capture().map(|output| output.stdout_str())
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))
}

#[rig_tool(
    description = "Write content to file at path",
    required(path, content)
)]
async fn write(path: std::path::PathBuf, content: String) -> Result<(), rig::tool::ToolError> {
    std::fs::write(&path, content)
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let client = deepseek::Client::from_env()?;

    let agent = client
        .agent(deepseek::DEEPSEEK_V4_PRO)
        .tool(Replace)
        .tool(Shell)
        .tool(Write)
        .max_tokens(100_000)
        .default_max_turns(100)
        .build();

    let response = agent.prompt("This rust project (in the cwd) is the agent framework within which you are running. Make it interactive (prompt, response, etc). Use crates to keep the project minimalist. I'm using git, so feel free to innovate -- I'll try again if I don't like the result.").await?;

    println!("{response}");

    Ok(())
}
