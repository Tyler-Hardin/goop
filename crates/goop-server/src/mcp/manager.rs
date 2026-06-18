//! MCP server lifecycle: connect, discover tools, hold peer handles.
//!
//! [`McpManager`] is created once per session (or shared across sessions
//! for `shared = true` servers).  It connects to each configured MCP
//! server — either via stdio subprocess or HTTP — performs the
//! initialization handshake, discovers all tools, and builds a
//! `Vec<Box<dyn ToolDyn>>` with one entry per discovered MCP tool.

use std::sync::Arc;

use rig::tool::ToolDyn;

use rmcp::model::{ClientCapabilities, InitializeRequestParams, Tool};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ServiceExt};

use crate::config::{McpServerDef, McpTransport};

use super::proxy_tool::McpProxyTool;

// ── service handle (sum type — one variant per transport) ──────────

/// Owns a live MCP connection. The type parameter differs between
/// HTTP and stdio, so we use an enum rather than type erasure.
#[allow(dead_code)]
enum McpService {
    Http(RunningService<RoleClient, InitializeRequestParams>),
    Stdio(RunningService<RoleClient, ClientHandlerImpl>),
}

// ── server handle ────────────────────────────────────────────────────

/// A live connection to one MCP server, carrying its discovered tools.
struct McpServerHandle {
    /// Config name for display / disambiguation.
    name: String,
    /// The MCP peer — clonable, used to call tools.
    peer: Peer<RoleClient>,
    /// Tools discovered from this server.
    tools: Vec<Tool>,
    /// Keeps the service alive; dropping it shuts down the connection.
    #[allow(dead_code)]
    _service: McpService,
}

impl std::fmt::Debug for McpServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerHandle")
            .field("name", &self.name)
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

// ── client handler (minimum viable) ──────────────────────────────────

/// Minimal [`ClientHandler`] — accepts all defaults.
/// Used only for stdio transport.
#[derive(Debug, Clone)]
struct ClientHandlerImpl;

impl ClientHandler for ClientHandlerImpl {
    // All methods use the default impls from the trait.
}

// ── manager ──────────────────────────────────────────────────────────

/// Manages connections to all configured MCP servers.
///
/// Holds the [`Peer`] for each server so tools can be called later.
/// Call [`McpManager::build_tools`] to produce one [`ToolDyn`] per
/// discovered MCP tool.
pub(crate) struct McpManager {
    /// Live server connections — each holds its own tool list.
    handles: Vec<McpServerHandle>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("servers", &self.handles.len())
            .field(
                "tools",
                &self.handles.iter().map(|h| h.tools.len()).sum::<usize>(),
            )
            .finish()
    }
}

impl McpManager {
    /// An empty manager with no servers — used as a sentinel before
    /// [`init_global_mcp`] runs, or for sessions with no MCP servers.
    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self {
            handles: Vec::new(),
        })
    }

    /// Connect to all configured MCP servers and discover their tools.
    ///
    /// Each server is connected via its configured transport.  Connection
    /// errors are logged and that server is skipped — one misconfigured
    /// server doesn't prevent the session from starting.
    ///
    /// `servers` maps config names to their definitions.
    pub async fn connect(servers: &[(String, McpServerDef)]) -> Arc<Self> {
        let mut handles = Vec::new();

        for (name, def) in servers {
            let result = match &def.transport {
                McpTransport::Http { .. } => Self::connect_http(name, def).await,
                McpTransport::Stdio { .. } => Self::connect_stdio(name, def).await,
            };

            match result {
                Ok(handle) => {
                    tracing::info!("MCP {name} — {} tool(s)", handle.tools.len());
                    handles.push(handle);
                }
                Err(e) => {
                    tracing::warn!("MCP {name} — failed: {e}");
                }
            }
        }

        Arc::new(Self { handles })
    }

    /// Build one [`ToolDyn`] per discovered MCP tool.
    ///
    /// Each tool is named `server_name.tool_name` to avoid collisions
    /// between servers that expose tools with identical names.
    pub fn build_tools(&self) -> Vec<Box<dyn ToolDyn>> {
        let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();

        for handle in &self.handles {
            for tool in &handle.tools {
                let qualified_name = format!("{}.{}", handle.name, tool.name);
                tools.push(Box::new(McpProxyTool::new(
                    qualified_name,
                    handle.peer.clone(),
                    tool.clone(),
                )));
            }
        }

        tools
    }

    // ── transports ─────────────────────────────────────────────────

    async fn connect_http(name: &str, def: &McpServerDef) -> anyhow::Result<McpServerHandle> {
        let McpTransport::Http { url } = &def.transport else {
            anyhow::bail!("connect_http called on non-HTTP server");
        };

        let transport = StreamableHttpClientTransport::from_uri(url.as_str());

        let client_info = InitializeRequestParams::new(
            ClientCapabilities::default(),
            rmcp::model::Implementation::new(
                "goop",
                option_env!("CARGO_PKG_VERSION").unwrap_or("0.0.0"),
            ),
        );

        let service = client_info
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("HTTP connect to {url}: {e}"))?;

        let peer = service.peer().clone();
        let tools: Vec<Tool> = peer
            .list_all_tools()
            .await
            .map_err(|e| anyhow::anyhow!("list_tools from {url}: {e}"))?;

        Ok(McpServerHandle {
            name: name.to_string(),
            peer,
            tools,
            _service: McpService::Http(service),
        })
    }

    async fn connect_stdio(name: &str, def: &McpServerDef) -> anyhow::Result<McpServerHandle> {
        let McpTransport::Stdio { command, args, env } = &def.transport else {
            anyhow::bail!("connect_stdio called on non-stdio server");
        };

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        cmd.env_remove("MCP_SERVER_NAME");
        cmd.env_remove("MCP_SERVER_VERSION");
        for (k, v) in env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd)?;
        let service = ClientHandlerImpl.serve(transport).await?;
        let peer = service.peer().clone();
        let tools: Vec<Tool> = peer.list_all_tools().await?;

        Ok(McpServerHandle {
            name: name.to_string(),
            peer,
            tools,
            _service: McpService::Stdio(service),
        })
    }
}

// ── result formatting (public for proxy_tool) ────────────────────────

/// Convert an MCP [`CallToolResult`] into a human-readable string.
pub(crate) fn format_mcp_result(result: &rmcp::model::CallToolResult) -> String {
    use rmcp::model::{RawContent, ResourceContents};

    let mut parts: Vec<String> = Vec::new();

    for item in &result.content {
        match &item.raw {
            RawContent::Text(text) => {
                parts.push(text.text.clone());
            }
            RawContent::Image(image) => {
                parts.push(format!(
                    "[image: {} bytes, mime: {}]",
                    image.data.len(),
                    image.mime_type,
                ));
            }
            RawContent::Audio(_) => {
                parts.push("[audio]".into());
            }
            RawContent::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    parts.push(format!("[resource {uri}: {}]", text));
                }
                ResourceContents::BlobResourceContents { uri, .. } => {
                    parts.push(format!("[resource {uri} (blob)]"));
                }
                #[allow(unreachable_patterns)]
                _ => {
                    parts.push("[resource (unknown)]".into());
                }
            },
            RawContent::ResourceLink(_link) => {
                parts.push("[resource link]".into());
            }
            #[allow(unreachable_patterns)]
            _ => {
                parts.push("[unknown content type]".into());
            }
        }
    }

    if parts.is_empty() {
        String::from("(empty result)")
    } else {
        parts.join("\n")
    }
}
