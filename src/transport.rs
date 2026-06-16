//! Transport abstraction for local vs. remote (SSH) file operations.
//!
//! A session starts with [`Transport::Local`]. The `ssh` tool promotes it
//! to [`Transport::Ssh`]; `disconnect` demotes it back.  File tools
//! (`read`, `write`, `replace`, `read_html`, `shell`, `cd`) route through
//! the transport so they transparently work on the remote host.
//!
//! Transport state is stored in [`SessionState`](crate::session_state::SessionState)
//! — there are no longer per-session global registries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::Handler;
use russh_sftp::client::SftpSession;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

// ── SSH handler ───────────────────────────────────────────────────

/// russh client handler that checks host keys against `~/.ssh/known_hosts`.
///
/// On first connection to an unknown host, the key is learned (TOFU).
/// If the key has changed since the last connection, the connection is
/// rejected.
pub(crate) struct SshHandler {
    /// Hostname we are connecting to (for known_hosts lookup).
    pub(crate) host: String,
    /// Port we are connecting to (for known_hosts lookup; non-22 ports use
    /// `[host]:port` format).
    pub(crate) port: u16,
}

impl Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        match russh::keys::known_hosts::check_known_hosts(&self.host, self.port, server_public_key)
        {
            Ok(true) => {
                tracing::info!("SSH host key verified for {}:{}", self.host, self.port);
                Ok(true)
            }
            Ok(false) => {
                // First time seeing this host — learn the key (TOFU).
                tracing::info!("learning new SSH host key for {}:{}", self.host, self.port);
                match russh::keys::known_hosts::learn_known_hosts(
                    &self.host,
                    self.port,
                    server_public_key,
                ) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        tracing::warn!(
                            "failed to write known_hosts for {}:{}: {e}",
                            self.host,
                            self.port
                        );
                        // Accept anyway rather than brick the connection.
                        Ok(true)
                    }
                }
            }
            Err(russh::keys::Error::KeyChanged { line }) => {
                // Key mismatch — potential MITM attack!
                tracing::error!(
                    "SSH host key CHANGED for {}:{} (known_hosts line {line})! \
                     If this is expected (e.g. server reinstall), remove the \
                     old key from ~/.ssh/known_hosts and reconnect.",
                    self.host,
                    self.port,
                );
                Err(russh::Error::KeyChanged { line })
            }
            Err(e) => {
                // Something else went wrong (e.g. NoHomeDir, I/O error).
                // Be lenient and accept.
                tracing::warn!(
                    "could not check known_hosts for {}:{}: {e}; accepting key",
                    self.host,
                    self.port,
                );
                Ok(true)
            }
        }
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ── SSH connection state ───────────────────────────────────────────

/// Holds the SSH connection, SFTP session, remote CWD, and remote home
/// directory for a session.
pub struct SshState {
    /// The russh handle, used for opening exec channels (shell commands).
    pub(crate) handle: Mutex<russh::client::Handle<SshHandler>>,
    /// High-level SFTP session for file read/write/directory operations.
    pub(crate) sftp: Mutex<SftpSession>,
    /// Current working directory on the remote host.
    pub remote_cwd: Mutex<PathBuf>,
    /// Remote user's home directory (set once on connect, never changes).
    pub remote_home_dir: PathBuf,
    /// Display name for the connection (e.g. "user@host:22").
    pub label: String,
}

// ── transport enum ─────────────────────────────────────────────────

/// Where file operations and shell commands execute.
#[derive(Clone)]
pub enum Transport {
    Local,
    Ssh(Arc<SshState>),
}

impl Transport {
    /// Whether this transport is an active SSH connection.
    pub fn is_ssh(&self) -> bool {
        matches!(self, Transport::Ssh(_))
    }

    /// Returns a human-readable label for the transport.
    pub fn label(&self) -> String {
        match self {
            Transport::Local => String::from("local"),
            Transport::Ssh(state) => state.label.clone(),
        }
    }

    /// Snapshot the transport into its serializable form.
    ///
    /// Runtime handles (SSH connection, SFTP session) are dropped —
    /// only the data needed to reconnect later is preserved.
    pub async fn to_persisted(&self) -> PersistedTransport {
        match self {
            Transport::Local => PersistedTransport::Local,
            Transport::Ssh(ssh) => PersistedTransport::Ssh {
                destination: ssh.label.clone(),
                remote_cwd: ssh.remote_cwd.lock().await.clone(),
            },
        }
    }
}

// ── persisted transport (serializable snapshot) ──────────────────

/// Serializable snapshot of a [`Transport`] for persistence.
///
/// The runtime handles are stripped — on deserialization, the SSH
/// connection is re-established from the [`PersistedTransport::Ssh`]
/// fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[derive(Default)]
pub enum PersistedTransport {
    #[serde(rename = "local")]
    #[default]
    Local,
    #[serde(rename = "ssh")]
    Ssh {
        /// e.g. "user@host:22"
        destination: String,
        /// Current working directory on the remote host.
        remote_cwd: PathBuf,
    },
}

// ── helpers ────────────────────────────────────────────────────────

/// Convert a path to a String for SFTP operations.
fn path_to_string(path: &Path) -> Result<String, anyhow::Error> {
    path.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("invalid path (non-UTF8): {}", path.display()))
}

// ── transport operations ───────────────────────────────────────────

impl Transport {
    /// Read the contents of a file (local or remote).
    pub async fn read_file(&self, path: &Path) -> Result<String, anyhow::Error> {
        match self {
            Transport::Local => Ok(tokio::fs::read_to_string(path).await?),
            Transport::Ssh(state) => {
                let sftp = state.sftp.lock().await;
                let path_str = path_to_string(path)?;
                let mut file = sftp
                    .open_with_flags(path_str, russh_sftp::protocol::OpenFlags::READ)
                    .await?;
                let mut content = String::new();
                file.read_to_string(&mut content).await?;
                Ok(content)
            }
        }
    }

    /// Write content to a file, creating or truncating it.
    pub async fn write_file(&self, path: &Path, content: &str) -> Result<(), anyhow::Error> {
        match self {
            Transport::Local => {
                tokio::fs::write(path, content).await?;
                Ok(())
            }
            Transport::Ssh(state) => {
                let sftp = state.sftp.lock().await;
                let path_str = path_to_string(path)?;
                let mut file = sftp
                    .open_with_flags(
                        path_str,
                        russh_sftp::protocol::OpenFlags::CREATE
                            | russh_sftp::protocol::OpenFlags::TRUNCATE
                            | russh_sftp::protocol::OpenFlags::WRITE,
                    )
                    .await?;
                file.write_all(content.as_bytes()).await?;
                file.shutdown().await?;
                Ok(())
            }
        }
    }

    /// Run a shell command and return combined stdout + stderr.
    ///
    /// `cwd` is the working directory in which the command executes
    /// (local path for local transport, remote path for SSH).
    pub async fn run_shell(&self, command: &str, cwd: &Path) -> Result<String, anyhow::Error> {
        match self {
            Transport::Local => {
                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(cwd)
                    .output()
                    .await?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                Ok(if stderr.is_empty() {
                    stdout.into_owned()
                } else {
                    format!("{stdout}{stderr}")
                })
            }
            Transport::Ssh(state) => {
                // Build a command that cds to the right directory first,
                // and redirects stderr to stdout so we only need one reader.
                let cwd_str = cwd.to_string_lossy();
                let escaped = shell_escape::unix::escape(cwd_str);
                let full_cmd = format!("cd {escaped} && {{ {command}; }} 2>&1");

                let handle = state.handle.lock().await;
                let mut channel = handle.channel_open_session().await?;
                channel.exec(true, full_cmd.as_bytes().to_vec()).await?;

                // Close stdin.
                channel.eof().await?;

                // Read stdout (which now includes stderr).
                let output = {
                    let mut reader = channel.make_reader();
                    read_to_string(&mut reader).await.unwrap_or_default()
                }; // reader dropped here, mutable borrow released

                // Wait for the remote process to finish.
                let _ = channel.wait().await;

                Ok(output)
            }
        }
    }

    /// Canonicalize a path (resolve `..`, `.`, symlinks).
    pub async fn canonicalize(&self, path: &Path) -> Result<PathBuf, anyhow::Error> {
        match self {
            Transport::Local => Ok(std::fs::canonicalize(path)?),
            Transport::Ssh(state) => {
                let sftp = state.sftp.lock().await;
                let path_str = path_to_string(path)?;
                let result: String = sftp.canonicalize(&path_str).await?;
                Ok(PathBuf::from(result))
            }
        }
    }

    /// Check whether a path exists and is a directory.
    pub async fn is_dir(&self, path: &Path) -> Result<bool, anyhow::Error> {
        match self {
            Transport::Local => Ok(path.is_dir()),
            Transport::Ssh(state) => {
                let sftp = state.sftp.lock().await;
                let path_str = path_to_string(path)?;
                match sftp.metadata(&path_str).await {
                    Ok(meta) => Ok(meta.is_dir()),
                    Err(_) => Ok(false),
                }
            }
        }
    }
}

// ── helper: read AsyncRead to String ───────────────────────────────

async fn read_to_string<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<String, anyhow::Error> {
    let mut buf = String::new();
    reader.read_to_string(&mut buf).await?;
    Ok(buf)
}
