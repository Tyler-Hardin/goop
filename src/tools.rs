use rig_derive::rig_tool;

#[rig_tool(
    description = "Replace old_str with new_str in file at path. old_str must be unique.",
    required(command)
)]
pub async fn replace(
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
pub async fn shell(command: String) -> Result<String, rig::tool::ToolError> {
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
pub async fn write(
    path: std::path::PathBuf,
    content: String,
) -> Result<String, rig::tool::ToolError> {
    tokio::fs::write(&path, &content)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    Ok(format!("Wrote {} bytes to {}", content.len(), path.display()))
}
