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
        let existing = this.state.transport();
        if existing.is_ssh() {
            let local_cwd = this.state.cwd();
            let local_cwd_path = crate::session::sessions_dir()
                .join(format!("{}.cwd.local", this.state.name));
            crate::session::save_cwd(&local_cwd_path, &local_cwd);
            this.state.set_transport(Transport::Local);
        } else {
            let local_cwd = this.state.cwd();
            let local_cwd_path = crate::session::sessions_dir()
                .join(format!("{}.cwd.local", this.state.name));
            crate::session::save_cwd(&local_cwd_path, &local_cwd);
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

        let cwd_path = crate::session::sessions_dir().join(format!("{}.cwd", this.state.name));
        crate::session::save_cwd(&cwd_path, &remote_cwd);

        let ssh_file = crate::session::sessions_dir().join(format!("{}.ssh", this.state.name));
        let _ = std::fs::write(
            &ssh_file,
            format!("{}\n{}\n", args.destination, remote_cwd.display()),
        );

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

        let ssh_file = crate::session::sessions_dir().join(format!("{}.ssh", this.state.name));
        let _ = std::fs::remove_file(&ssh_file);

        let local_cwd_path = crate::session::sessions_dir()
            .join(format!("{}.cwd.local", this.state.name));
        let local_cwd = if let Ok(contents) = std::fs::read_to_string(&local_cwd_path) {
            let p = PathBuf::from(contents.trim());
            if p.is_dir() {
                let _ = std::fs::remove_file(&local_cwd_path);
                p
            } else {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            }
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };

        this.state.set_cwd(local_cwd.clone());

        let cwd_path = crate::session::sessions_dir().join(format!("{}.cwd", this.state.name));
        crate::session::save_cwd(&cwd_path, &local_cwd);

        Ok(format!("Disconnected — now operating locally in {}", local_cwd.display()))
    }
);
