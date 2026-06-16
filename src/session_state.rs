//! Per-session state shared with tools — replaces the former
//! `SESSION_CWDS` / `SESSION_TRANSPORTS` globals and `SESSION_ID` task-local.
//!
//! Each [`Session`] owns an `Arc<SessionState>`.  Tools receive a clone and
//! read/write CWD and transport through it.
//!
//! ## Persistence
//!
//! [`PersistedSessionState`] is the on-disk snapshot, stored at
//! `~/.config/goop/sessions/<name>.state.toml`.  It bundles:
//!
//! - Per-session config overrides ([`SessionConfig`](crate::config::SessionConfig))
//! - Local CWD (always tracked, even when SSH'd)
//! - Transport state (local vs. SSH destination + remote CWD)
//!
//! The runtime [`SessionState`] is initialised from this snapshot and
//! written back whenever CWD or transport changes.

use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Serialize};

use crate::config::SessionConfig;
use crate::transport::Transport;

// ── runtime session state ───────────────────────────────────────────

/// Shared, mutable per-session state accessible by tools.
pub struct SessionState {
    /// Session name (user-supplied or auto-generated).
    pub name: String,
    /// User's home directory (from [`Config`](crate::config::Config)).
    pub home_dir: PathBuf,
    /// Current working directory for this session.
    pub cwd: StdMutex<PathBuf>,
    /// Current transport (local or SSH).
    pub transport: StdMutex<Transport>,
    /// Path to the persisted state file (for save operations).
    state_path: PathBuf,
}

impl SessionState {
    pub fn new(name: String, home_dir: PathBuf, cwd: PathBuf, state_path: PathBuf) -> Self {
        Self {
            name,
            home_dir,
            cwd: StdMutex::new(cwd),
            transport: StdMutex::new(Transport::Local),
            state_path,
        }
    }

    /// Persist the current CWD and transport to `<name>.state.toml`.
    ///
    /// Preserves any existing config overrides in the file — only the
    /// CWD and transport sections are updated.
    pub fn save(&self) {
        let mut persisted = PersistedSessionState::load_from(&self.state_path).unwrap_or_default();

        // Update the mutable fields.
        persisted.local_cwd = match self.transport() {
            Transport::Local => self.cwd(),
            Transport::Ssh(_) => {
                // Keep the existing local_cwd from the file (it's the
                // pre-SSH local CWD).  Only update if we're local.
                persisted.local_cwd
            }
        };
        persisted.transport = TransportState::from_transport(&self.transport());

        let _ = persisted.save_to(&self.state_path);
    }

    // ── convenience accessors ──────────────────────────────────────

    /// Snapshot of the current CWD.
    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }

    /// Replace the CWD.  Returns the previous value.
    pub fn set_cwd(&self, path: PathBuf) -> PathBuf {
        std::mem::replace(&mut *self.cwd.lock().unwrap(), path)
    }

    /// Snapshot of the current transport.
    pub fn transport(&self) -> Transport {
        self.transport.lock().unwrap().clone()
    }

    /// Replace the transport.  Returns the previous value.
    pub fn set_transport(&self, t: Transport) -> Transport {
        std::mem::replace(&mut *self.transport.lock().unwrap(), t)
    }

    // ── helpers used by tools ─────────────────────────────────────

    /// Resolve a possibly-relative path against the session CWD.
    pub fn resolve_path(&self, path: PathBuf) -> PathBuf {
        if path.is_absolute() {
            path
        } else {
            self.cwd().join(path)
        }
    }

    /// Expand `~` and `~/…` prefixes using `home_dir`.
    pub fn expand_tilde(&self, path: &str) -> PathBuf {
        if path == "~" || path == "~/" {
            self.home_dir.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home_dir.join(rest)
        } else {
            PathBuf::from(path)
        }
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
    pub transport: TransportState,
}

fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

impl Default for PersistedSessionState {
    fn default() -> Self {
        Self {
            config: SessionConfig::default(),
            local_cwd: default_cwd(),
            transport: TransportState::default(),
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
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Write to the standard location for a named session.
    #[allow(dead_code)]
    pub fn save(&self, name: &str) -> anyhow::Result<()> {
        self.save_to(&state_path(name))
    }
}

// ── transport state (serializable) ──────────────────────────────────

/// Serializable snapshot of the current transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[derive(Default)]
pub enum TransportState {
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

impl TransportState {
    /// Build a [`TransportState`] from the runtime [`Transport`].
    pub fn from_transport(t: &Transport) -> Self {
        match t {
            Transport::Local => TransportState::Local,
            Transport::Ssh(state) => {
                let remote_cwd = state
                    .remote_cwd
                    .try_lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                TransportState::Ssh {
                    destination: state.label.clone(),
                    remote_cwd,
                }
            }
        }
    }
}

// ── path helpers ────────────────────────────────────────────────────

/// Path to the session state file: `~/.config/goop/sessions/<name>.state.toml`
pub fn state_path(name: &str) -> PathBuf {
    crate::config::config_dir()
        .join("sessions")
        .join(format!("{name}.state.toml"))
}
