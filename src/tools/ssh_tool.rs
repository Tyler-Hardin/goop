//! SSH tools: `ssh`, `disconnect`.
//!
//! Thin wrappers around [`SessionState::ssh_connect`] and
//! [`SessionState::ssh_disconnect`].

use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::{define_tool, tool_err};

define_tool!(pub(crate) struct Ssh, args = SshArgs,
    tool_name: "ssh",
    desc: "Connect to a remote server via SSH. All subsequent file operations (read, write, replace, read_html) and shell commands will execute on the remote host. Use 'disconnect' to return to local operation. Example destination: 'user@host' or 'user@host:2222'.",
    params: serde_json::json!({
        "type": "object",
        "properties": {
            "destination": { "type": "string", "description": "user@host or user@host:port" },
            "password": { "type": "string", "description": "Optional SSH password" }
        },
        "required": ["destination"]
    }),
    args { destination: String, password: Option<String> },
    |this, args| this.state.ssh_connect(args.destination, args.password).await.map_err(tool_err)
);

define_tool!(pub(crate) struct Disconnect,
    tool_name: "disconnect",
    desc: "Close the SSH connection and return to local operation. File operations and shell commands will run on the local machine again.",
    params: serde_json::json!({ "type": "object", "properties": {} }),
    |this| this.state.ssh_disconnect().await.map_err(tool_err)
);
