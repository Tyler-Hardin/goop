//! Per-MCP-tool proxy: implements [`rig::tool::ToolDyn`] directly (not
//! [`Tool`]) so the tool name can be dynamic — one proxy per discovered
//! MCP tool, each with its own name, schema, and dispatch peer.

use std::pin::Pin;
use std::sync::Arc;

use futures::Future;
use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};

use rmcp::model::Tool;
use rmcp::service::{Peer, RoleClient};

use super::manager::format_mcp_result;

/// A single MCP tool exposed to the agent as a [`ToolDyn`].
///
/// Implements [`ToolDyn`] directly rather than [`Tool`](rig::tool::Tool)
/// so the tool name can be set at runtime (avoiding the `const NAME`
/// limitation).
pub(crate) struct McpProxyTool {
    /// Fully-qualified name: `server_name.tool_name`.
    name: String,
    /// Clone of the MCP server peer — used to call tools.
    peer: Peer<RoleClient>,
    /// MCP tool metadata (description, input schema).
    mcp_tool: Tool,
}

impl McpProxyTool {
    pub fn new(name: String, peer: Peer<RoleClient>, mcp_tool: Tool) -> Self {
        Self {
            name,
            peer,
            mcp_tool,
        }
    }
}

impl ToolDyn for McpProxyTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn definition(
        &self,
        _prompt: String,
    ) -> Pin<Box<dyn Future<Output = ToolDefinition> + Send + '_>> {
        let name = self.name.clone();
        let desc = self
            .mcp_tool
            .description
            .clone()
            .unwrap_or(std::borrow::Cow::from(""))
            .into_owned();
        let params = self.mcp_tool.schema_as_json_value();

        Box::pin(async move {
            ToolDefinition {
                name,
                description: desc,
                parameters: params,
            }
        })
    }

    fn call(
        &self,
        args: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let peer = self.peer.clone();
        let tool_name: Arc<str> = self.mcp_tool.name.clone().into();

        Box::pin(async move {
            // Parse args as a JSON object map, forwarding to MCP.
            let arguments: rmcp::model::JsonObject =
                if args.trim().is_empty() || args.trim() == "null" {
                    serde_json::Map::new()
                } else {
                    serde_json::from_str(&args).unwrap_or_else(|_| serde_json::Map::new())
                };

            let params = rmcp::model::CallToolRequestParams::new(tool_name.as_ref().to_string())
                .with_arguments(arguments);

            let result = peer.call_tool(params).await.map_err(|e| {
                ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string())))
            })?;

            Ok(format_mcp_result(&result))
        })
    }
}
