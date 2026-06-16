//! Server restart tool: `restart`.
//!
//! Schedules a graceful server restart after the current prompt
//! completes.  Only available when the session's CWD is the goop
//! source tree (detected via the `AGENTS.md` marker).

use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::define_tool;

define_tool!(pub(crate) struct Restart, args = RestartArgs,
    tool_name: "restart",
    desc: "Schedule a graceful goop server restart after the current \
           prompt completes.  Call this after recompiling the binary \
           (e.g. via `cargo build`).  Clients will briefly disconnect \
           and reconnect to the new process.",
    params: serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    }),
    args {},
    |this, _args| {
        crate::server::trigger_restart();
        Ok("Server restart scheduled — will take effect after this prompt completes.".into())
    }
);
