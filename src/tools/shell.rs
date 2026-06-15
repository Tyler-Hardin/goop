//! Shell tool: `shell`.

use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::{define_tool, tool_err};

define_tool!(pub(crate) struct Shell, args = ShellArgs,
    tool_name: "shell",
    desc: "Run command in shell",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Shell command to run" }
        },
        "required": ["command"]
    }),
    args { command: String },
    |this, args| {
        let transport = this.state.transport();
        let cwd = this.state.cwd();
        transport
            .run_shell(&args.command, &cwd)
            .await
            .map_err(tool_err)
    }
);
