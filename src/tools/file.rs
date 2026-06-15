//! File-operation tools: `read`, `write`, `replace`, `read_html`, `cd`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::{define_tool, tool_err};
use crate::transport::Transport;

// ═══════════════════════════════════════════════════════════════════
// Read
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct Read, args = ReadArgs,
    tool_name: "read",
    desc: "Read file at path, optionally with start_line and end_line (both 1-indexed, inclusive). Returns line-numbered content.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file" },
            "start_line": { "type": "integer", "description": "Optional start line (1-indexed, inclusive)" },
            "end_line": { "type": "integer", "description": "Optional end line (1-indexed, inclusive)" }
        },
        "required": ["path"]
    }),
    args { path: PathBuf, start_line: Option<u64>, end_line: Option<u64> },
    |this, args| {
        let transport = this.state.transport();
        let path = this.state.resolve_path(args.path);
        let content = transport.read_file(&path).await.map_err(tool_err)?;

        let all_lines: Vec<&str> = content.lines().collect();
        let total = all_lines.len() as u64;

        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(total).min(total);

        if start > total {
            return Err(tool_err(format!("start_line {start} exceeds file length ({total} lines)")));
        }
        if start > end {
            return Err(tool_err(format!("start_line {start} > end_line {end}")));
        }

        let output: String = all_lines[(start - 1) as usize..end as usize]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", start as usize + i, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(output)
    }
);

// ═══════════════════════════════════════════════════════════════════
// Write
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct Write, args = WriteArgs,
    tool_name: "write",
    desc: "Write content to file at path",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file" },
            "content": { "type": "string", "description": "Content to write" }
        },
        "required": ["path", "content"]
    }),
    args { path: PathBuf, content: String },
    |this, args| {
        let transport = this.state.transport();
        let path = this.state.resolve_path(args.path);
        let len = args.content.len();
        transport.write_file(&path, &args.content).await.map_err(tool_err)?;
        Ok(format!("Wrote {len} bytes to {}", path.display()))
    }
);

// ═══════════════════════════════════════════════════════════════════
// Replace
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct Replace, args = ReplaceArgs,
    tool_name: "replace",
    desc: "Replace old_str with new_str in file at path. old_str must be unique.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file" },
            "old_str": { "type": "string", "description": "String to find (must be unique)" },
            "new_str": { "type": "string", "description": "Replacement string" }
        },
        "required": ["path", "old_str", "new_str"]
    }),
    args { path: PathBuf, old_str: String, new_str: String },
    |this, args| {
        let transport = this.state.transport();
        let path = this.state.resolve_path(args.path);
        let content = transport.read_file(&path).await.map_err(tool_err)?;
        let count = content.matches(&args.old_str).count();
        if count == 0 {
            Err(tool_err("old_str not found"))
        } else if count > 1 {
            Err(tool_err(format!("old_str found {count} times, must be unique")))
        } else {
            let new_content = content.replacen(&args.old_str, &args.new_str, 1);
            transport.write_file(&path, &new_content).await.map_err(tool_err)?;
            Ok(format!("Replaced 1 occurrence in {}", path.display()))
        }
    }
);

// ═══════════════════════════════════════════════════════════════════
// ReadHtml
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct ReadHtml, args = ReadHtmlArgs,
    tool_name: "read_html",
    desc: "Read an HTML file at path and return extracted plain text (headings, links, body text). Useful for local crate docs, cached pages, etc.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the HTML file" }
        },
        "required": ["path"]
    }),
    args { path: PathBuf },
    |this, args| {
        let transport = this.state.transport();
        let path = this.state.resolve_path(args.path);
        let html = transport.read_file(&path).await.map_err(tool_err)?;
        tokio::task::spawn_blocking(move || {
            html2text::from_read(html.as_bytes(), 80).map_err(|e| tool_err(e.to_string()))
        })
        .await
        .map_err(|e| tool_err(e.to_string()))?
    }
);

// ═══════════════════════════════════════════════════════════════════
// Cd
// ═══════════════════════════════════════════════════════════════════

define_tool!(pub(crate) struct Cd, args = CdArgs,
    tool_name: "cd",
    desc: "Change the session's working directory. Affects all future shell, read, write, and other file operations. The path can be absolute, relative (to current CWD), '~' for home, or '..' for parent. Returns the new absolute path.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "New working directory path" }
        },
        "required": ["path"]
    }),
    args { path: String },
    |this, args| {
        let transport = this.state.transport();
        let current = this.state.cwd();

        let new_path = if args.path.starts_with('~') {
            this.state.expand_tilde(&args.path)
        } else if args.path.starts_with('/') {
            PathBuf::from(&args.path)
        } else {
            current.join(&args.path)
        };

        let canonical = transport
            .canonicalize(&new_path)
            .await
            .map_err(|e| tool_err(format!("cd: {}: {}", new_path.display(), e)))?;

        if !transport
            .is_dir(&canonical)
            .await
            .map_err(|e| tool_err(format!("cd: {}: {}", canonical.display(), e)))?
        {
            return Err(tool_err(format!("cd: not a directory: {}", canonical.display())));
        }

        this.state.set_cwd(canonical.clone());

        if let Transport::Ssh(ref ssh_state) = transport {
            *ssh_state.remote_cwd.lock().await = canonical.clone();
        }

        let cwd_path = crate::session::sessions_dir().join(format!("{}.cwd", this.state.name));
        crate::session::save_cwd(&cwd_path, &canonical);

        Ok(format!("Changed working directory to {}", canonical.display()))
    }
);
