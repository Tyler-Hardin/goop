//! SSH connection management with `~/.ssh/config` support, key
//! authentication, and ProxyJump tunnelling.
//!
//! ## Architecture
//!
//! ```
//! ssh_connect("myserver", password)
//!   → parse ~/.ssh/config
//!   → resolve Host → HostName, User, Port, IdentityFile, ProxyJump
//!   → if ProxyJump: connect to jump → open direct-tcpip → connect_stream to target
//!   → try IdentityFile keys (and default ~/.ssh/id_*)
//!   → fall back to password if provided
//!   → open SFTP channel
//!   → return Transport::Ssh(…)
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use tokio::sync::Mutex;

use crate::transport::{SshHandler, SshState, Transport};

// ── SSH config ───────────────────────────────────────────────────────

/// A single `Host` block from an OpenSSH config file.
#[derive(Debug, Clone, Default)]
struct HostBlock {
    /// The patterns from the `Host` line (e.g. `["myserver", "*.example.com"]`).
    patterns: Vec<String>,
    /// Key-value options (keys are lowercased).
    options: HashMap<String, Vec<String>>,
}

/// Parsed `~/.ssh/config`.
#[derive(Debug, Clone, Default)]
struct SshConfig {
    blocks: Vec<HostBlock>,
}

impl SshConfig {
    /// Parse `~/.ssh/config` into structured blocks.
    fn load() -> Result<Self, anyhow::Error> {
        let path = ssh_config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        Ok(Self::parse(&content))
    }

    fn parse(input: &str) -> Self {
        let mut blocks: Vec<HostBlock> = Vec::new();
        let mut current: Option<HostBlock> = None;

        for raw in input.lines() {
            let line = raw.trim();

            // Skip blanks and comments.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // A Host/Match line starts a new block.
            let lower = line.to_lowercase();
            if lower.starts_with("host ") || lower == "host" {
                if let Some(block) = current.take()
                    && !block.patterns.is_empty()
                {
                    blocks.push(block);
                }
                let patterns = line[4..]
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                current = Some(HostBlock {
                    patterns,
                    ..Default::default()
                });
                continue;
            }

            // "Match" also starts a block. We skip Match blocks for now.
            if lower.starts_with("match ") || lower == "match" {
                if let Some(block) = current.take()
                    && !block.patterns.is_empty()
                {
                    blocks.push(block);
                }
                // Push a placeholder so options inside Match are ignored.
                current = None;
                continue;
            }

            // Regular key-value line.
            if let Some(ref mut block) = current
                && let Some((key, value)) = parse_kv(line)
            {
                block
                    .options
                    .entry(key.to_lowercase())
                    .or_default()
                    .push(value);
            }
            // Lines outside any Host block are global and ignored for now.
        }

        if let Some(block) = current
            && !block.patterns.is_empty()
        {
            blocks.push(block);
        }

        SshConfig { blocks }
    }

    /// Resolve a hostname: find the first matching `Host` block and return
    /// merged options.  If no block matches, returns an empty map.
    fn resolve(&self, host: &str) -> HashMap<String, Vec<String>> {
        for block in &self.blocks {
            if block.matches(host) {
                return block.options.clone();
            }
        }
        HashMap::new()
    }
}

impl HostBlock {
    /// Check whether any pattern in this block matches `host`.
    fn matches(&self, host: &str) -> bool {
        self.patterns
            .iter()
            .any(|pat| glob::Pattern::new(pat).is_ok_and(|p| p.matches(host)))
    }
}

/// Parse a `key value` / `key = value` / `key=value` line.
fn parse_kv(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    // Try "key = value" or "key value" first.
    if let Some(pos) = line.find(char::is_whitespace) {
        let key = &line[..pos];
        let rest = line[pos..].trim_start();
        let value = rest.strip_prefix('=').map(|r| r.trim()).unwrap_or(rest);
        if !key.is_empty() && !value.is_empty() {
            return Some((key.to_string(), value.to_string()));
        }
    }
    // Try "key=value" (no whitespace).
    if let Some(pos) = line.find('=') {
        let key = &line[..pos];
        let value = line[pos + 1..].trim();
        if !key.is_empty() && !value.is_empty() {
            return Some((key.to_string(), value.to_string()));
        }
    }
    None
}

fn ssh_config_path() -> PathBuf {
    crate::config::home_dir().join(".ssh").join("config")
}

// ── Resolved connection parameters ───────────────────────────────────

/// Fully-resolved connection parameters after consulting `~/.ssh/config`.
#[derive(Debug, Clone)]
struct ResolvedHost {
    host: String,
    port: u16,
    user: String,
    identity_files: Vec<PathBuf>,
    proxy_jumps: Vec<String>,
}

/// Resolve a destination string (`[user@]host[:port]`) against SSH config.
fn resolve_destination(destination: &str) -> Result<ResolvedHost, anyhow::Error> {
    // Parse the raw destination into user, host, optional port.
    let (raw_user, raw_host, raw_port) = parse_destination(destination);

    // Look up in SSH config.
    let config = SshConfig::load().unwrap_or_default();
    let opts = config.resolve(&raw_host);

    let host = opts
        .get("hostname")
        .and_then(|v| v.first().cloned())
        .unwrap_or(raw_host);

    let port = raw_port
        .or_else(|| {
            opts.get("port")
                .and_then(|v| v.first()?.parse::<u16>().ok())
        })
        .unwrap_or(22);

    let user = raw_user
        .or_else(|| opts.get("user").and_then(|v| v.first().cloned()))
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| String::from("root")));

    let identity_files: Vec<PathBuf> = opts
        .get("identityfile")
        .map(|v| v.iter().map(|s| expand_ssh_tilde(s)).collect())
        .unwrap_or_default();

    let proxy_jumps: Vec<String> = opts
        .get("proxyjump")
        .map(|v| {
            v.iter()
                .flat_map(|s| s.split(','))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s != "none")
                .collect()
        })
        .unwrap_or_default();

    Ok(ResolvedHost {
        host,
        port,
        user,
        identity_files,
        proxy_jumps,
    })
}

/// Parse `[user@]host[:port]` into (user, host, port).
fn parse_destination(dest: &str) -> (Option<String>, String, Option<u16>) {
    let (user, host_port) = if let Some((u, hp)) = dest.split_once('@') {
        (Some(u.to_string()), hp.to_string())
    } else {
        (None, dest.to_string())
    };

    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h.to_string(), Some(p.parse::<u16>().unwrap_or(22)))
    } else {
        (host_port, None)
    };

    (user, host, port)
}

// ── Key loading ──────────────────────────────────────────────────────

/// Load a single private key from disk.  Returns `None` if the file
/// doesn't exist (so callers can skip non-existent default keys).
fn load_key(path: &Path) -> Result<Option<Arc<russh::keys::PrivateKey>>, anyhow::Error> {
    if !path.exists() {
        return Ok(None);
    }
    match russh::keys::load_secret_key(path, None) {
        Ok(key) => Ok(Some(Arc::new(key))),
        Err(e) => {
            tracing::debug!("failed to load key {}: {e}", path.display());
            Ok(None)
        }
    }
}

/// Default key paths to try: `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`,
/// `~/.ssh/id_ecdsa`.
fn default_identity_files() -> Vec<PathBuf> {
    let ssh = crate::config::home_dir().join(".ssh");
    vec![
        ssh.join("id_ed25519"),
        ssh.join("id_rsa"),
        ssh.join("id_ecdsa"),
    ]
}

// ── Authentication ───────────────────────────────────────────────────

/// Try to authenticate with a keypair.
async fn try_key_auth(
    handle: &mut russh::client::Handle<SshHandler>,
    user: &str,
    key: &Arc<russh::keys::PrivateKey>,
) -> Result<bool, russh::Error> {
    let keypair = russh::keys::key::PrivateKeyWithHashAlg::new(key.clone(), None);
    let result = handle.authenticate_publickey(user, keypair).await?;
    Ok(result.success())
}

/// Attempt authentication: keys first (both configured and default), then
/// password if provided.
async fn authenticate(
    handle: &mut russh::client::Handle<SshHandler>,
    resolved: &ResolvedHost,
    password: Option<&str>,
) -> Result<(), anyhow::Error> {
    // Collect all identity files to try.
    let mut key_paths = resolved.identity_files.clone();
    if key_paths.is_empty() {
        key_paths = default_identity_files();
    }

    // Try each key.
    for key_path in &key_paths {
        if let Some(key) = load_key(key_path)? {
            tracing::debug!("trying key: {}", key_path.display());
            match try_key_auth(handle, &resolved.user, &key).await {
                Ok(true) => {
                    tracing::info!("authenticated with key: {}", key_path.display());
                    return Ok(());
                }
                Ok(false) => {
                    tracing::debug!("key rejected: {}", key_path.display());
                }
                Err(e) => {
                    tracing::warn!("key auth error for {}: {e}", key_path.display());
                }
            }
        }
    }

    // Fall back to password.
    if let Some(pw) = password {
        tracing::debug!("trying password auth for {}", resolved.user);
        let result = handle.authenticate_password(&resolved.user, pw).await?;
        if result.success() {
            return Ok(());
        }
    }

    Err(anyhow::anyhow!(
        "SSH authentication failed for {}@{}:{} — tried {} key(s){}",
        resolved.user,
        resolved.host,
        resolved.port,
        key_paths.len(),
        if password.is_some() {
            ", plus password"
        } else {
            " (no password provided)"
        }
    ))
}

// ── ProxyJump tunnelling ─────────────────────────────────────────────

/// Connect to `target` through a chain of jump hosts.
///
/// Each element of `jumps` is a destination string like `user@host:port`.
/// We connect to the first, open a direct-tcpip tunnel to the second,
/// run SSH over that tunnel, and repeat until we reach the final target.
async fn connect_via_jumps(
    jumps: &[String],
    resolved: &ResolvedHost,
    password: Option<&str>,
) -> Result<
    (
        russh::client::Handle<SshHandler>,
        SftpSession,
        PathBuf,
        PathBuf,
    ),
    anyhow::Error,
> {
    if jumps.is_empty() {
        return direct_connect(resolved, password).await;
    }

    // Resolve and connect to the first jump.
    let first = resolve_destination(&jumps[0])?;
    let mut tunnel = russh_connect(&first, password).await?;

    // Chain through remaining jumps.
    let remaining = &jumps[1..];
    let final_target = if remaining.is_empty() {
        (resolved.host.clone(), resolved.port)
    } else {
        let next = resolve_destination(&remaining[0])?;
        (next.host, next.port)
    };

    let stream = open_tunnel(&mut tunnel, &final_target.0, final_target.1).await?;

    if remaining.is_empty() {
        // We tunnelled to the target. Now connect SSH over the tunnel.
        connect_over_tunnel(stream, resolved, password).await
    } else {
        // We tunnelled to the next jump. Now connect SSH over the tunnel and
        // recurse through the rest. Use the identity files from the next jump's config.
        let next_resolved = resolve_destination(&remaining[0])?;
        let (mut next_handle, _, _, _) =
            connect_over_tunnel(stream, &next_resolved, password).await?;

        // Recurse: tunnel from here through the remaining jumps to the target.
        let final_stream = open_tunnel(&mut next_handle, &resolved.host, resolved.port).await?;
        connect_over_tunnel(final_stream, resolved, password).await
    }
}

/// Open a direct-tcpip channel to `host:port` and return the channel stream.
async fn open_tunnel(
    handle: &mut russh::client::Handle<SshHandler>,
    host: &str,
    port: u16,
) -> Result<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static, anyhow::Error>
{
    let channel = handle
        .channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0)
        .await?;
    Ok(channel.into_stream())
}

/// Connect SSH over an existing stream (used for ProxyJump tunnels).
async fn connect_over_tunnel(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    resolved: &ResolvedHost,
    password: Option<&str>,
) -> Result<
    (
        russh::client::Handle<SshHandler>,
        SftpSession,
        PathBuf,
        PathBuf,
    ),
    anyhow::Error,
> {
    let config = Arc::new(russh::client::Config::default());
    let handler = SshHandler {
        host: resolved.host.clone(),
        port: resolved.port,
    };
    let mut handle = russh::client::connect_stream(config, stream, handler).await?;

    // Authenticate using the resolved identity files (or defaults).
    let key_paths = if resolved.identity_files.is_empty() {
        default_identity_files()
    } else {
        resolved.identity_files.clone()
    };
    let mut authed = false;
    for key_path in &key_paths {
        if let Some(key) = load_key(key_path)?
            && let Ok(true) = try_key_auth(&mut handle, &resolved.user, &key).await
        {
            authed = true;
            break;
        }
    }
    if !authed {
        if let Some(pw) = password {
            let result = handle.authenticate_password(&resolved.user, pw).await?;
            if !result.success() {
                return Err(anyhow::anyhow!(
                    "ProxyJump tunnel authentication failed for {}@{}",
                    resolved.user,
                    resolved.host
                ));
            }
        } else {
            return Err(anyhow::anyhow!(
                "ProxyJump tunnel authentication failed for {}@{} (no password, no keys)",
                resolved.user,
                resolved.host
            ));
        }
    }

    // Open SFTP.
    let channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream()).await?;

    let remote_cwd_str: String = sftp.canonicalize(".").await?;
    let remote_cwd = PathBuf::from(remote_cwd_str);

    let home = remote_home_dir(&mut handle).await?;

    Ok((handle, sftp, remote_cwd, home))
}

/// Direct TCP connection (no ProxyJump).
async fn direct_connect(
    resolved: &ResolvedHost,
    password: Option<&str>,
) -> Result<
    (
        russh::client::Handle<SshHandler>,
        SftpSession,
        PathBuf,
        PathBuf,
    ),
    anyhow::Error,
> {
    let config = Arc::new(russh::client::Config::default());
    let handler = SshHandler {
        host: resolved.host.clone(),
        port: resolved.port,
    };
    let mut handle =
        russh::client::connect(config, (resolved.host.as_str(), resolved.port), handler).await?;

    authenticate(&mut handle, resolved, password).await?;

    let channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream()).await?;

    let remote_cwd_str: String = sftp.canonicalize(".").await?;
    let remote_cwd = PathBuf::from(remote_cwd_str);

    let home = remote_home_dir(&mut handle).await?;

    Ok((handle, sftp, remote_cwd, home))
}

/// Connect to a host via russh (used for ProxyJump chain).
async fn russh_connect(
    resolved: &ResolvedHost,
    password: Option<&str>,
) -> Result<russh::client::Handle<SshHandler>, anyhow::Error> {
    let config = Arc::new(russh::client::Config::default());
    let handler = SshHandler {
        host: resolved.host.clone(),
        port: resolved.port,
    };
    let mut handle =
        russh::client::connect(config, (resolved.host.as_str(), resolved.port), handler).await?;

    authenticate(&mut handle, resolved, password).await?;

    Ok(handle)
}

// ── Public interface ─────────────────────────────────────────────────

/// Connect to a remote host via SSH and return a [`Transport`].
///
/// `destination` is in `user@host[:port]` format (or just `host`).  The
/// destination is looked up in `~/.ssh/config` to resolve `HostName`,
/// `User`, `Port`, `IdentityFile`, and `ProxyJump`.
///
/// Authentication tries all configured (or default) identity files first,
/// then falls back to `password` if provided.
pub async fn ssh_connect(
    destination: &str,
    password: Option<&str>,
) -> Result<Transport, anyhow::Error> {
    let resolved = resolve_destination(destination)?;
    let label = format!("{}@{}:{}", resolved.user, resolved.host, resolved.port);

    let (handle, sftp, remote_cwd, remote_home) = if !resolved.proxy_jumps.is_empty() {
        connect_via_jumps(&resolved.proxy_jumps, &resolved, password).await?
    } else {
        direct_connect(&resolved, password).await?
    };

    Ok(Transport::Ssh(Arc::new(SshState {
        handle: Mutex::new(handle),
        sftp: Mutex::new(sftp),
        remote_cwd: Mutex::new(remote_cwd),
        remote_home_dir: remote_home,
        label,
    })))
}

// ── helpers ──────────────────────────────────────────────────────────

/// Expand `~` and `~/…` using the local home directory (for SSH config
/// paths like `IdentityFile`, which are always local).
fn expand_ssh_tilde(s: &str) -> PathBuf {
    let home = crate::config::home_dir();
    if s == "~" {
        home
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Run `echo $HOME` on the remote host to discover the user's home
/// directory.  Returns an error if the output is empty or the command fails.
async fn remote_home_dir(
    handle: &mut russh::client::Handle<SshHandler>,
) -> Result<PathBuf, anyhow::Error> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, b"echo $HOME".to_vec()).await?;
    channel.eof().await?;
    let output = {
        let mut reader = channel.make_reader();
        read_to_string(&mut reader).await.unwrap_or_default()
    };
    let _ = channel.wait().await;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("remote echo \\$HOME returned empty output"));
    }
    Ok(PathBuf::from(trimmed))
}

async fn read_to_string<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<String, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut buf = String::new();
    reader.read_to_string(&mut buf).await?;
    Ok(buf)
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_basic() {
        let input = r#"
Host myserver
    HostName 10.0.0.1
    User admin
    Port 2222
    IdentityFile ~/.ssh/mykey
"#;
        let config = SshConfig::parse(input);
        assert_eq!(config.blocks.len(), 1);
        let block = &config.blocks[0];
        assert_eq!(block.patterns, vec!["myserver"]);
        assert_eq!(
            block
                .options
                .get("hostname")
                .and_then(|v| v.first().map(|s| s.as_str())),
            Some("10.0.0.1")
        );
        assert_eq!(
            block
                .options
                .get("user")
                .and_then(|v| v.first().map(|s| s.as_str())),
            Some("admin")
        );
        assert_eq!(
            block
                .options
                .get("port")
                .and_then(|v| v.first().map(|s| s.as_str())),
            Some("2222")
        );
        assert_eq!(
            block
                .options
                .get("identityfile")
                .and_then(|v| v.first().map(|s| s.as_str())),
            Some("~/.ssh/mykey")
        );
    }

    #[test]
    fn test_parse_config_wildcard() {
        let input = r#"
Host *.example.com
    HostName %h
    User deploy
"#;
        let config = SshConfig::parse(input);
        assert_eq!(config.blocks.len(), 1);
        assert_eq!(config.blocks[0].patterns, vec!["*.example.com"]);
    }

    #[test]
    fn test_parse_config_multiple_hosts() {
        let input = r#"
Host a b c
    HostName shared
"#;
        let config = SshConfig::parse(input);
        assert_eq!(config.blocks.len(), 1);
        assert_eq!(config.blocks[0].patterns, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_config_equals_sign() {
        let input = r#"
Host s
    HostName = foo
    Port=33
"#;
        let config = SshConfig::parse(input);
        let opts = &config.blocks[0].options;
        assert_eq!(
            opts.get("hostname")
                .and_then(|v| v.first().map(|s| s.as_str())),
            Some("foo")
        );
        assert_eq!(
            opts.get("port").and_then(|v| v.first().map(|s| s.as_str())),
            Some("33")
        );
    }

    #[test]
    fn test_parse_config_ignores_match() {
        let input = r#"
Host keep
    HostName kept
Match all
    HostName ignored
Host another
    HostName anotherhost
"#;
        let config = SshConfig::parse(input);
        assert_eq!(config.blocks.len(), 2);
        assert_eq!(config.blocks[0].patterns, vec!["keep"]);
        assert_eq!(config.blocks[1].patterns, vec!["another"]);
        // The "Match all" block and its options are dropped.
    }

    #[test]
    fn test_parse_config_proxyjump() {
        let input = r#"
Host target
    HostName internal.example.com
    ProxyJump bastion.example.com
"#;
        let config = SshConfig::parse(input);
        let opts = &config.blocks[0].options;
        assert_eq!(
            opts.get("proxyjump")
                .and_then(|v| v.first().map(|s| s.as_str())),
            Some("bastion.example.com")
        );
    }

    #[test]
    fn test_expand_tilde() {
        let home = crate::config::home_dir();
        assert_eq!(expand_ssh_tilde("~"), home);
        assert_eq!(expand_ssh_tilde("~/foo"), home.join("foo"));
        assert_eq!(expand_ssh_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
