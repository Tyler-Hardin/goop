//! File-operation tools: `read`, `write`, `replace`, `read_html`, `cd`.
//!
//! Each tool is a thin wrapper — it deserializes arguments and delegates
//! to the corresponding [`SessionState`] method.  No tool touches CWD,
//! transport, or path resolution directly.

use std::path::PathBuf;
use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::{define_tool, tool_err};

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
    |this, args| this.state.read_file(args.path, args.start_line, args.end_line).await.map_err(tool_err)
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
    |this, args| this.state.write_file(args.path, args.content).await.map_err(tool_err)
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
    |this, args| this.state.replace_in_file(args.path, args.old_str, args.new_str).await.map_err(tool_err)
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
    |this, args| this.state.read_html(args.path).await.map_err(tool_err)
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
    |this, args| this.state.change_dir(args.path).await.map_err(tool_err)
);
