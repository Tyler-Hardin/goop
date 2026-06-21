//! Per-session state: CWD, transport, and file/shell/SSH operations.
//!
//! ## Design
//!
//! [`SessionState`] is the single authority for all stateful operations.
//! Tools are thin wrappers — they deserialize arguments and delegate to a
//! public method here.  No tool ever locks a mutex, resolves a path, or
//! touches the transport directly.
//!
//! State is held in a single [`tokio::sync::Mutex<SessionStateInner>`].
//! There is no contention (tools run sequentially), so fine-grained
//! locking would only add lock-ordering ceremony with no benefit.
//! I/O (network, filesystem) always happens outside the lock — callers
//! clone the transport handle, drop the lock, then do I/O.
//!
//! ## CWD design
//!
//! CWD is derived from the current transport:
//! - **Local:** CWD comes from [`SessionStateInner::local_cwd`].
//! - **SSH:**   CWD comes from [`SshState::remote_cwd`].
//!
//! There is no duplication — `set_cwd` routes to the correct backing
//! field automatically.  `local_cwd` is never lost when SSH'd, so
//! `ssh_disconnect` reads it from memory (no disk I/O).
//!
//! ## Persistence
//!
//! [`PersistedSessionState`] is the on-disk snapshot at
//! `~/.config/goop/sessions/<name>.state.toml`.  `save()` writes purely
//! from memory — no read-modify-write cycle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use goop_shared::SessionConfig;
use crate::transport::{PersistedTransport, Transport, TransportError};

// ── error type ───────────────────────────────────────────────────────

/// Errors that can occur during session-level operations (file I/O,
/// directory changes, shell execution, SSH).
#[derive(thiserror::Error, Debug)]
pub enum SessionStateError {
    /// An error from the transport layer (I/O, SFTP, SSH channel).
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// Filesystem I/O error (e.g. writing the state file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error when persisting session state.
    #[error("serialization error: {0}")]
    Toml(#[from] toml::ser::Error),

    /// SSH connection or authentication failure.
    #[error("SSH error: {0}")]
    Ssh(String),

    /// A path-related error from `cd`: canonicalization failure,
    /// path is not a directory, etc.
    #[error("cd error: {0}")]
    Cd(String),

    /// Line-range arguments are out of bounds for the file.
    #[error("invalid line range: start={start}, end={end}, file has {total} lines")]
    LineRange { start: u64, end: u64, total: u64 },

    /// `old_str` was either not found, or found more than once.
    #[error("replace error: {0}")]
    Replace(String),

    /// A spawned blocking task panicked or was cancelled.
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// HTML-to-text conversion failed.
    #[error("html2text conversion failed: {0}")]
    Html2Text(String),

    /// A web fetch failed (HTTP error, curl missing on remote, DNS, etc.).
    #[error("web fetch failed: {0}")]
    WebFetch(String),
}

/// Alias for results with [`SessionStateError`].
pub type SessionResult<T> = std::result::Result<T, SessionStateError>;

// ── inner state (single lock) ──────────────────────────────────────

/// All mutable session fields behind one lock — the state product.
struct SessionStateInner {
    local_cwd: PathBuf,
    transport: Transport,
    session_config: SessionConfig,
    /// Active branch tip, mirrored from the [`TransactionLog`](crate::TransactionLog)
    /// for persistence.  `None` = "last entry" (linear default).  The log is
    /// the source of truth for replay; this mirror is written to
    /// `<name>.state.toml` by [`save`](SessionState::save) and kept in sync by
    /// the session when a branch switch happens.
    active_tip: Option<u64>,
}

impl SessionStateInner {
    /// Home directory from the transport's perspective (sync — no I/O).
    fn transport_home_dir(&self) -> PathBuf {
        match &self.transport {
            Transport::Ssh(ssh) => ssh.remote_home_dir.clone(),
            Transport::Local => PathBuf::new(), // filled in by caller
        }
    }
}

// ── runtime session state ───────────────────────────────────────────

/// Shared, mutable per-session state.
pub struct SessionState {
    /// Local user home directory — always the machine goop is running on.
    local_home_dir: PathBuf,
    /// All mutable state behind a single lock.  No contention, so a
    /// single Mutex is both sufficient and simpler than fine-grained
    /// locking with documented ordering conventions.
    inner: Mutex<SessionStateInner>,
    /// Path to the persisted state file.
    state_path: PathBuf,
}

impl SessionState {
    pub fn new(
        local_home_dir: PathBuf,
        initial_local_cwd: PathBuf,
        session_config: SessionConfig,
        state_path: PathBuf,
    ) -> Self {
        Self {
            local_home_dir,
            inner: Mutex::new(SessionStateInner {
                local_cwd: initial_local_cwd,
                transport: Transport::Local,
                session_config,
                active_tip: None,
            }),
            state_path,
        }
    }

    /// Read the current session config overrides.
    pub(crate) async fn session_config(&self) -> SessionConfig {
        self.inner.lock().await.session_config.clone()
    }

    /// Replace the session config overrides and persist to disk.
    /// Called by [`apply_settings`](crate::session::Session::apply_settings)
    /// so settings changes survive server restarts.
    pub(crate) async fn set_session_config(&self, config: SessionConfig) {
        self.inner.lock().await.session_config = config;
        self.save().await;
    }

    /// Set the persisted active-tip mirror (called by the session on a branch
    /// switch).  Does not save — the caller persists via [`save`](Self::save).
    pub(crate) async fn set_active_tip(&self, tip: Option<u64>) {
        self.inner.lock().await.active_tip = tip;
    }

    // ── public operations (called by tools) ────────────────────────

    /// Read a file, optionally with line-range slicing.
    ///
    /// `path` is resolved relative to the session CWD.  `start_line` and
    /// `end_line` are 1-indexed and inclusive.
    pub async fn read_file(
        &self,
        path: PathBuf,
        start_line: Option<u64>,
        end_line: Option<u64>,
    ) -> SessionResult<String> {
        let (transport, resolved) = self.resolve_io_context(&path).await?;
        let content = transport.read_file(&resolved).await?;

        let all_lines: Vec<&str> = content.lines().collect();
        let total = all_lines.len() as u64;

        let start = start_line.unwrap_or(1).max(1);
        let end = end_line.unwrap_or(total).min(total);

        if start > total {
            return Err(SessionStateError::LineRange { start, end, total });
        }
        if start > end {
            return Err(SessionStateError::LineRange { start, end, total });
        }

        Ok(all_lines[(start - 1) as usize..end as usize]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", start as usize + i, line))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Write content to a file (create or truncate).
    ///
    /// `path` is resolved relative to the session CWD.
    pub async fn write_file(&self, path: PathBuf, content: String) -> SessionResult<String> {
        let (transport, resolved) = self.resolve_io_context(&path).await?;
        let len = content.len();
        transport.write_file(&resolved, &content).await?;
        Ok(format!("Wrote {len} bytes to {}", resolved.display()))
    }

    /// Replace `old_str` with `new_str` in a file.  `old_str` must
    /// appear exactly once in the file.
    ///
    /// `path` is resolved relative to the session CWD.
    pub async fn replace_in_file(
        &self,
        path: PathBuf,
        old_str: String,
        new_str: String,
    ) -> SessionResult<String> {
        let (transport, resolved) = self.resolve_io_context(&path).await?;
        let content = transport.read_file(&resolved).await?;
        let count = content.matches(&old_str).count();
        if count == 0 {
            return Err(SessionStateError::Replace("old_str not found".into()));
        }
        if count > 1 {
            return Err(SessionStateError::Replace(format!(
                "old_str found {count} times, must be unique"
            )));
        }
        let new_content = content.replacen(&old_str, &new_str, 1);
        transport.write_file(&resolved, &new_content).await?;
        Ok(format!("Replaced 1 occurrence in {}", resolved.display()))
    }

    /// Read an HTML file and extract plain text (headings, links, body).
    ///
    /// `path` is resolved relative to the session CWD.
    pub async fn read_html(&self, path: PathBuf) -> SessionResult<String> {
        let (transport, resolved) = self.resolve_io_context(&path).await?;
        let html = transport.read_file(&resolved).await?;
        tokio::task::spawn_blocking(move || html2text::from_read(html.as_bytes(), 80))
            .await?
            .map_err(|e| SessionStateError::Html2Text(e.to_string()))
    }

    /// Change the session's working directory.
    ///
    /// `path` may be absolute, relative (to current CWD), `~` for home,
    /// or `..` for parent.  The result is canonicalised and verified to
    /// be a directory before the CWD is updated.
    pub async fn change_dir(&self, path: String) -> SessionResult<String> {
        // Snapshot everything we need, then drop the lock for I/O.
        let (transport, current_cwd, home) = {
            let inner = self.inner.lock().await;
            let t = inner.transport.clone();
            let cwd = self.cwd_of(&inner).await;
            let h = if matches!(inner.transport, Transport::Local) {
                self.local_home_dir.clone()
            } else {
                inner.transport_home_dir()
            };
            (t, cwd, h)
        };

        let new_path = if path.starts_with('~') {
            if path == "~" || path == "~/" {
                home
            } else if let Some(rest) = path.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(&path)
            }
        } else if path.starts_with('/') {
            PathBuf::from(&path)
        } else {
            current_cwd.join(&path)
        };

        // I/O outside lock.
        let canonical = transport
            .canonicalize(&new_path)
            .await
            .map_err(|e| SessionStateError::Cd(format!("{}: {e}", new_path.display())))?;

        if !transport
            .is_dir(&canonical)
            .await
            .map_err(|e| SessionStateError::Cd(format!("{}: {e}", canonical.display())))?
        {
            return Err(SessionStateError::Cd(format!(
                "not a directory: {}",
                canonical.display()
            )));
        }

        self.set_cwd(canonical.clone()).await;
        self.save().await;

        Ok(format!(
            "Changed working directory to {}",
            canonical.display()
        ))
    }

    /// Run a shell command in the session's CWD (local or remote).
    pub async fn run_shell(&self, command: String) -> SessionResult<String> {
        let (transport, cwd) = {
            let inner = self.inner.lock().await;
            let t = inner.transport.clone();
            let c = self.cwd_of(&inner).await;
            (t, c)
        };
        transport
            .run_shell(&command, &cwd)
            .await
            .map_err(SessionStateError::Transport)
    }

    /// Connect to a remote host via SSH.
    ///
    /// All subsequent file and shell operations will execute on the
    /// remote host.  If already SSH'd to a different host, disconnects
    /// first (local CWD is preserved).
    ///
    /// `destination` is `user@host` or `user@host:port` format.
    pub async fn ssh_connect(
        &self,
        destination: String,
        password: Option<String>,
    ) -> SessionResult<String> {
        // If already SSH'd, go back to local first.
        {
            let mut inner = self.inner.lock().await;
            if inner.transport.is_ssh() {
                inner.transport = Transport::Local;
            }
        }

        let transport = crate::ssh::ssh_connect(&destination, password.as_deref())
            .await
            .map_err(|e| SessionStateError::Ssh(e.to_string()))?;

        // Use the current (local) CWD as the initial remote CWD.
        // canonicalize resolves it on the remote side.
        let current_cwd = {
            let inner = self.inner.lock().await;
            inner.local_cwd.clone()
        };
        let remote_cwd = transport
            .canonicalize(&current_cwd)
            .await
            .unwrap_or_else(|_| PathBuf::from("."));

        let label = transport.label();
        {
            let mut inner = self.inner.lock().await;
            inner.transport = transport;
        }
        // set_cwd routes to ssh_state.remote_cwd because transport is now Ssh.
        self.set_cwd(remote_cwd.clone()).await;
        self.save().await;

        Ok(format!(
            "Connected to {label} — working directory: {}",
            remote_cwd.display()
        ))
    }

    /// Close the SSH connection and return to local operation.
    ///
    /// Restores the local CWD that was active before `ssh_connect`.
    /// No-ops (with a message) if already local.
    pub async fn ssh_disconnect(&self) -> SessionResult<String> {
        let local_cwd = {
            let mut inner = self.inner.lock().await;
            if !inner.transport.is_ssh() {
                return Ok("Not connected via SSH — already operating locally.".into());
            }
            inner.transport = Transport::Local;
            inner.local_cwd.clone()
        };

        self.set_cwd(local_cwd.clone()).await;
        self.save().await;

        Ok(format!(
            "Disconnected — now operating locally in {}",
            local_cwd.display()
        ))
    }

    /// Fetch a URL and extract plain text from the HTML.
    ///
    /// On the local transport this uses `reqwest` directly.  On SSH it
    /// runs `curl -sSf` on the remote host — so the URL is fetched from
    /// the *remote's* network perspective (internal services, VPNs, etc.
    /// are reachable).  The raw HTML and extracted text are cached to
    /// `/tmp/goop/` so the LLM can re-examine them with `read` or
    /// `read_html`.
    pub async fn web_fetch(&self, url: String) -> SessionResult<String> {
        let (transport, is_local) = {
            let inner = self.inner.lock().await;
            (
                inner.transport.clone(),
                matches!(inner.transport, Transport::Local),
            )
        };

        if is_local {
            self.web_fetch_local(&url).await
        } else {
            self.web_fetch_remote(&transport, &url).await
        }
    }

    // ── pub(crate) — used by Session::new for SSH reconnect ──────

    /// Replace the current transport.  Returns the previous value.
    pub(crate) async fn set_transport(&self, t: Transport) -> Transport {
        std::mem::replace(&mut self.inner.lock().await.transport, t)
    }

    /// Persist current CWD, transport, and session config to disk.
    /// Writes purely from memory — no read-modify-write cycle.
    pub(crate) async fn save(&self) {
        let persisted = {
            let inner = self.inner.lock().await;
            let transport = inner.transport.to_persisted().await;
            PersistedSessionState {
                config: inner.session_config.clone(),
                local_cwd: inner.local_cwd.clone(),
                transport,
                active_tip: inner.active_tip,
            }
        }; // lock dropped — disk I/O outside critical section.
        let _ = persisted.save_to(&self.state_path);
    }

    // ── private helpers ───────────────────────────────────────────

    /// Resolve `path` against the session CWD and return the transport
    /// handle + resolved path.  The transport handle is cloned out of
    /// the lock so I/O happens without holding it.
    async fn resolve_io_context(&self, path: &Path) -> SessionResult<(Transport, PathBuf)> {
        let inner = self.inner.lock().await;
        let transport = inner.transport.clone();
        let cwd = self.cwd_of(&inner).await;
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        Ok((transport, resolved))
    }

    /// CWD derived from the current transport.  `inner` must already
    /// be locked by the caller.
    async fn cwd_of(&self, inner: &SessionStateInner) -> PathBuf {
        match &inner.transport {
            Transport::Local => inner.local_cwd.clone(),
            Transport::Ssh(ssh) => ssh.remote_cwd.lock().await.clone(),
        }
    }

    /// Set CWD, routing to the correct backing field.
    async fn set_cwd(&self, path: PathBuf) {
        let mut inner = self.inner.lock().await;
        match &inner.transport {
            Transport::Local => {
                inner.local_cwd = path;
            }
            Transport::Ssh(ssh) => {
                let ssh = Arc::clone(ssh);
                // Don't hold the outer lock while locking remote_cwd.
                // This is safe: the SshState is behind an Arc, so it
                // outlives the outer guard.
                drop(inner);
                *ssh.remote_cwd.lock().await = path;
            }
        }
    }

    // ── web_fetch helpers ─────────────────────────────────────────

    /// Fetch via `reqwest` on the local machine.
    async fn web_fetch_local(&self, url: &str) -> SessionResult<String> {
        let resp = reqwest::get(url)
            .await
            .map_err(|e| SessionStateError::WebFetch(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SessionStateError::WebFetch(format!("HTTP {status}")));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| SessionStateError::WebFetch(e.to_string()))?;

        let text = tokio::task::spawn_blocking({
            let html = html.clone();
            move || {
                html2text::from_read(html.as_bytes(), 80)
                    .map_err(|e| SessionStateError::Html2Text(e.to_string()))
            }
        })
        .await??;

        let dir = std::env::temp_dir().join("goop");
        tokio::fs::create_dir_all(&dir).await?;
        let stem = slugify(url);
        let txt_path = dir.join(format!("{stem}.txt"));
        let html_path = dir.join(format!("{stem}.html"));
        tokio::fs::write(&txt_path, &text).await?;
        tokio::fs::write(&html_path, &html).await?;

        Ok(format!(
            "{text}\n\n---\nCached: {} (plain text) and {} (raw HTML) — \
             use `read` or `read_html` or `shell` (e.g. grep) to re-examine if needed.",
            txt_path.display(),
            html_path.display(),
        ))
    }

    /// Fetch via `curl` on the remote host, then process locally.
    async fn web_fetch_remote(&self, transport: &Transport, url: &str) -> SessionResult<String> {
        // curl -sSf: silent, show errors, fail on HTTP 4xx/5xx.
        // The marker lets us distinguish success (HTML output) from
        // failure (curl error message on merged stderr).
        let marker = "GOOP_CURL_OK_7a3f1b9c";
        let cmd = format!("curl -sSf {url:?} && echo {marker:?}");
        let output = transport
            .run_shell(&cmd, &PathBuf::from("/tmp"))
            .await
            .map_err(SessionStateError::Transport)?;

        // Strip the marker to get raw HTML; if absent, curl failed.
        let html = match output.strip_suffix(&format!("{marker}\n")) {
            Some(html) => html.to_string(),
            None => {
                let err = output.trim();
                let detail = if err.is_empty() {
                    "curl produced no output — is curl installed on the remote host?"
                } else {
                    err
                };
                return Err(SessionStateError::WebFetch(detail.to_string()));
            }
        };

        // HTML → text conversion on the local machine.
        let text = tokio::task::spawn_blocking({
            let html = html.clone();
            move || {
                html2text::from_read(html.as_bytes(), 80)
                    .map_err(|e| SessionStateError::Html2Text(e.to_string()))
            }
        })
        .await??;

        // Write cached files to the remote host via transport.
        let remote_dir = PathBuf::from("/tmp/goop");
        // Ensure the directory exists on the remote.
        transport
            .run_shell("mkdir -p /tmp/goop", &PathBuf::from("/tmp"))
            .await
            .map_err(SessionStateError::Transport)?;

        let stem = slugify(url);
        let txt_path = remote_dir.join(format!("{stem}.txt"));
        let html_path = remote_dir.join(format!("{stem}.html"));
        transport.write_file(&txt_path, &text).await?;
        transport.write_file(&html_path, &html).await?;

        Ok(format!(
            "{text}\n\n---\nCached on remote: {} (plain text) and {} \
             (raw HTML) — use `read` or `read_html` to re-examine if needed.",
            txt_path.display(),
            html_path.display(),
        ))
    }
}

// ── persisted session state ─────────────────────────────────────────

/// On-disk snapshot of a session's mutable state.
///
/// Stored at `~/.config/goop/sessions/<name>.state.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSessionState {
    /// Per-session config overrides (all optional — merge into global).
    #[serde(default)]
    pub config: SessionConfig,
    /// Local working directory (always tracked, even when SSH'd).
    #[serde(default = "default_cwd")]
    pub local_cwd: PathBuf,
    /// Transport state.
    #[serde(default)]
    pub transport: PersistedTransport,
    /// The active branch tip (transaction-log seq) — which branch the session
    /// is on.  `None` (the default) means "last entry" (linear); set only when
    /// the user has switched to an older branch (future UI).  Basic forking
    /// (edit-and-regenerate) always continues on the newest branch, so this
    /// stays `None` and the parent pointers alone determine the active branch.
    /// See §2.9 of the redesign doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tip: Option<u64>,
}

fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

impl Default for PersistedSessionState {
    fn default() -> Self {
        Self {
            config: SessionConfig::default(),
            local_cwd: default_cwd(),
            transport: PersistedTransport::default(),
            active_tip: None,
        }
    }
}

impl PersistedSessionState {
    /// Load from a `<name>.state.toml` path.  Returns `None` if the file
    /// doesn't exist or is corrupt.
    pub fn load_from(path: &Path) -> Option<Self> {
        let contents = std::fs::read_to_string(path).ok()?;
        toml::from_str(&contents).ok()
    }

    /// Load a session's persisted state by name.
    pub fn load(name: &str) -> Option<Self> {
        Self::load_from(&state_path(name))
    }

    /// Write to a `<name>.state.toml` path (creates parent dirs).
    pub fn save_to(&self, path: &Path) -> std::result::Result<(), anyhow::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

// ── path helpers ────────────────────────────────────────────────────

/// Path to the session state file: `~/.config/goop/sessions/<name>.state.toml`
pub fn state_path(name: &str) -> PathBuf {
    crate::config::config_dir()
        .join("sessions")
        .join(format!("{name}.state.toml"))
}

// ── misc ──────────────────────────────────────────────────────────────

/// Check whether `cwd` is the goop source tree by looking for the
/// `AGENTS.md` marker file.  Used to gate the `restart` tool.
pub(crate) fn is_goop_project_dir(cwd: &Path) -> bool {
    std::fs::read_to_string(cwd.join("AGENTS.md"))
        .map(|s| s.starts_with("# AGENTS.md — goop"))
        .unwrap_or(false)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_goop_project_dir_detects_self() {
        // Walk up from the current directory to find the workspace root
        // (which contains AGENTS.md).  `cargo test` sets CWD to the crate
        // directory, which is nested inside the workspace.
        let cwd = std::env::current_dir().unwrap();
        let root = cwd
            .ancestors()
            .find(|p| p.join("AGENTS.md").exists())
            .unwrap_or(&cwd);
        assert!(
            is_goop_project_dir(root),
            "expected to detect goop project at {}",
            root.display()
        );
    }
}
