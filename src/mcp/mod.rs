//! MCP client subsystem — connects to external MCP servers and bridges
//! their tools into goop's agent via [`rig::tool::ToolDyn`].
//!
//! ## Design
//!
//! A single [`McpManager`] per session handles all configured MCP servers.
//! On session creation it connects via HTTP or spawns each server as a
//! child process, performs the MCP handshake
//! (`initialize` → `notify_initialized` → `tools/list`),
//! and holds the discovered tool metadata.
//!
//! [`McpManager::build_tools`] creates one [`McpProxyTool`] per discovered
//! MCP tool.  Each implements [`ToolDyn`](rig::tool::ToolDyn) directly
//! (not the `Tool` trait with its `const NAME` limitation) so tool names
//! are fully dynamic.  Tool names use `server_name.tool_name` format to
//! avoid collisions.
//!
//! ## Transports
//!
//! - **HTTP** (`type = "http"` in config): connects to a Streamable HTTP
//!   endpoint.
//! - **Stdio** (`type = "stdio"` in config): spawns a child process.

pub(crate) mod manager;
pub(crate) mod proxy_tool;

pub(crate) use manager::McpManager;

use crate::config::Config;

// ── resolution ──────────────────────────────────────────────────────

/// Resolve which per-session MCP servers to connect.
///
/// `enabled` = global `enabled_mcp_servers` ∪ session overrides.
/// Only non-`shared` servers are returned (shared servers are handled
/// by [`SessionManager::init_global_mcp`](crate::session::SessionManager::init_global_mcp)).
/// Servers not in the registry are logged and skipped.
pub(crate) fn resolve(config: &Config, session_names: &[String]) -> Vec<String> {
    let mut enabled: Vec<String> = config.enabled_mcp_servers.clone();
    for name in session_names {
        if !enabled.contains(name) {
            enabled.push(name.clone());
        }
    }

    let mut out = Vec::new();

    for name in &enabled {
        match config.mcp_servers.get(name) {
            Some(def) => {
                if !def.shared {
                    out.push(name.clone());
                }
            }
            None => {
                tracing::warn!(
                    "MCP server {name:?} is enabled but not defined in \
                     config.toml [mcp_servers] — skipping"
                );
            }
        }
    }

    out
}
