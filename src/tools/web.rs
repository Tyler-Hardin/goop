//! Web tool: `web_fetch`.

use std::sync::Arc;

use rig::tool::ToolError;

use crate::session_state::SessionState;
use crate::tools::define_tool;

define_tool!(pub(crate) struct WebFetch, args = WebFetchArgs,
    tool_name: "web_fetch",
    desc: "Fetch a URL and return extracted plain text from the HTML (headings, links, body text). Use for reading web docs, wiki pages, etc.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "URL to fetch" }
        },
        "required": ["url"]
    }),
    args { url: String },
    |this, args| {
        let resp = reqwest::get(&args.url)
            .await
            .map_err(|e| crate::tools::tool_err(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(crate::tools::tool_err(format!("HTTP {status}")));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| crate::tools::tool_err(e.to_string()))?;
        let text = tokio::task::spawn_blocking({
            let html = html.clone();
            move || {
                html2text::from_read(html.as_bytes(), 80)
                    .map_err(|e| ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string()))))
            }
        })
        .await
        .map_err(|e| crate::tools::tool_err(e.to_string()))??;

        let dir = std::env::temp_dir().join("goop");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| crate::tools::tool_err(e.to_string()))?;
        let stem = slugify(&args.url);
        let txt_path = dir.join(format!("{stem}.txt"));
        let html_path = dir.join(format!("{stem}.html"));
        tokio::fs::write(&txt_path, &text)
            .await
            .map_err(|e| crate::tools::tool_err(e.to_string()))?;
        tokio::fs::write(&html_path, &html)
            .await
            .map_err(|e| crate::tools::tool_err(e.to_string()))?;

        Ok(format!(
            "{text}\n\n---\nCached: {} (plain text) and {} (raw HTML) — use `read` or `read_html` or `shell` (e.g. grep) to re-examine if needed.",
            txt_path.display(),
            html_path.display(),
        ))
    }
);

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
