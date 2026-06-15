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

#[rig_tool(
    description = "Read an HTML file at path and return extracted plain text (headings, links, body text). Useful for local crate docs, cached pages, etc.",
    required(path)
)]
pub async fn read_html(path: std::path::PathBuf) -> Result<String, rig::tool::ToolError> {
    let html = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    tokio::task::spawn_blocking(move || {
        html2text::from_read(html.as_bytes(), 80).map_err(|e| {
            rig::tool::ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string())))
        })
    })
    .await
    .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?
}

#[rig_tool(
    description = "Fetch a URL and return extracted plain text from the HTML (headings, links, body text). Use for reading web docs, wiki pages, etc.",
    required(url)
)]
pub async fn web_fetch(url: String) -> Result<String, rig::tool::ToolError> {
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(rig::tool::ToolError::ToolCallError(Box::new(
            std::io::Error::other(format!("HTTP {status}")),
        )));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let text = tokio::task::spawn_blocking({
        let html = html.clone();
        move || {
            html2text::from_read(html.as_bytes(), 80).map_err(|e| {
                rig::tool::ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string())))
            })
        }
    })
    .await
    .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))??;

    // Write cached copies to temp files so the model can re-read with
    // the `read` or `read_html` tools without re-fetching.
    let dir = std::env::temp_dir().join("goop");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    let stem = slugify(&url);
    let txt_path = dir.join(format!("{stem}.txt"));
    let html_path = dir.join(format!("{stem}.html"));
    tokio::fs::write(&txt_path, &text)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;
    tokio::fs::write(&html_path, &html)
        .await
        .map_err(|e| rig::tool::ToolError::ToolCallError(Box::new(e)))?;

    Ok(format!(
        "{text}\n\n---\nCached: {} (plain text) and {} (raw HTML) — use `read` or `read_html` or `shell` (e.g. grep) to re-examine if needed.",
        txt_path.display(),
        html_path.display(),
    ))
}

/// Turn a URL into a safe filename fragment.
fn slugify(url: &str) -> String {
    url.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect::<String>()
        .chars()
        .take(120)
        .collect()
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
