//! SSH tools: `ssh`, `disconnect`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::session_state::SessionState;
use crate::tools::{define_tool, tool_err};
use crate::transport::Transport;

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
    |this, args| {
        // If already SSH'd, disconnect first so we save local state.
        if this.state.transport().is_ssh() {
            this.state.set_transport(Transport::Local);
        }

        let transport = crate::ssh::ssh_connect(&args.destination, args.password.as_deref())
            .await
            .map_err(|e| tool_err(format!("ssh: {e}")))?;

        let remote_cwd = {
            let cwd = this.state.cwd();
            transport
                .canonicalize(&cwd)
                .await
                .unwrap_or_else(|_| PathBuf::from("."))
        };

        this.state.set_transport(transport.clone());
        this.state.set_cwd(remote_cwd.clone());
        this.state.save();

        Ok(format!(
            "Connected to {} — working directory: {}",
            transport.label(),
            remote_cwd.display()
        ))
    }
);

define_tool!(pub(crate) struct Disconnect,
    tool_name: "disconnect",
    desc: "Close the SSH connection and return to local operation. File operations and shell commands will run on the local machine again.",
    params: serde_json::json!({ "type": "object", "properties": {} }),
    |this| {
        let transport = this.state.transport();
        if !transport.is_ssh() {
            return Ok("Not connected via SSH — already operating locally.".into());
        }

        this.state.set_transport(Transport::Local);

        // Restore local CWD from the persisted state.
        let local_cwd = crate::session_state::PersistedSessionState::load(&this.state.name)
            .map(|p| p.local_cwd)
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            });

        this.state.set_cwd(local_cwd.clone());
        this.state.save();

        Ok(format!("Disconnected — now operating locally in {}", local_cwd.display()))
    }
);
