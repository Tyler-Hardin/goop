//! Per-session state shared with tools — replaces the former
//! `SESSION_CWDS` / `SESSION_TRANSPORTS` globals and `SESSION_ID` task-local.
//!
//! Each [`Session`] owns an `Arc<SessionState>`.  Tools receive a clone and
//! read/write CWD and transport through it.

use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

use crate::transport::Transport;

/// Shared, mutable per-session state.
pub struct SessionState {
    /// Session name (user-supplied or auto-generated).
    pub name: String,
    /// User's home directory (from [`Config`](crate::config::Config)).
    pub home_dir: PathBuf,
    /// Current working directory for this session.
    pub cwd: StdMutex<PathBuf>,
    /// Current transport (local or SSH).
    pub transport: StdMutex<Transport>,
}

impl SessionState {
    pub fn new(name: String, home_dir: PathBuf, cwd: PathBuf) -> Self {
        Self {
            name,
            home_dir,
            cwd: StdMutex::new(cwd),
            transport: StdMutex::new(Transport::Local),
        }
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
