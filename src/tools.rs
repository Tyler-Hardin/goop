use rig_derive::rig_tool;

#[rig_tool(
    description = "Read file at path, optionally with start_line and end_line (both 1-indexed, inclusive). Returns line-numbered content.",
    required(command)
)]
pub async fn read(
    path: std::path::PathBuf,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> Result<String, rig::tool::ToolError> {
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;

    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len() as u64;

    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(total).min(total);

    if start > total {
        return Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!(
                "start_line {start} exceeds file length ({total} lines)"
            )),
        )));
    }

    if start > end {
        return Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!("start_line {start} > end_line {end}")),
        )));
    }

    let output: String = all_lines[(start - 1) as usize..end as usize]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start as usize + i, line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(output)
}

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
            std::io::Error::other(format!("old_str found {count} times, must be unique")),
        )))
    } else {
        let new_content = content.replacen(&old_str, &new_str, 1);
        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
        Ok(format!("Replaced 1 occurrence in {}", path.display()))
    }
}

#[rig_tool(description = "Run command in shell", required(command))]
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

#[rig_tool(description = "Write content to file at path", required(path, content))]
pub async fn write(
    path: std::path::PathBuf,
    content: String,
) -> Result<String, rig::tool::ToolError> {
    tokio::fs::write(&path, &content)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}
