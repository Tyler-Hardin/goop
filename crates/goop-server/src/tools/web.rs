//! Web tool: `web_fetch`.
//!
//! Thin wrapper around [`SessionState::web_fetch`].

use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::{define_tool, tool_err};

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
    |this, args| this.state.web_fetch(args.url).await.map_err(tool_err)
);
