//! Tools exposed to the LLM, grouped by function.
//!
//! Each tool struct implements [`rig::tool::Tool`] and receives an
//! `Arc<SessionState>` for accessing CWD, transport, and home_dir.
//!
//! Tool groups are gated by [`Config::enabled_tool_groups`](crate::config::Config).

pub(crate) mod computer;
pub(crate) mod file;
pub(crate) mod restart;
pub(crate) mod shell;
pub(crate) mod ssh_tool;
pub(crate) mod web;

use std::sync::Arc;

use rig::tool::{ToolDyn, ToolError};

use crate::config::{Config, ToolGroup};
use crate::session_state::SessionState;

// ── shared helpers ────────────────────────────────────────────────

/// Convert anyhow/display errors into [`ToolError`].
pub(crate) fn tool_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::ToolCallError(Box::new(std::io::Error::other(e.to_string())))
}

// ── tool definition macro ─────────────────────────────────────────

/// Define a tool struct with args struct, [`Tool`] impl, and constructor.
///
/// ```ignore
/// define_tool!(pub(crate) struct Read, args = ReadArgs,
///     tool_name: "read",
///     desc: "Read file at path…",
///     params: json!({…}),
///     args { path: PathBuf, start_line: Option<u64>, end_line: Option<u64> },
///     |this, args| { … }
/// );
/// ```
///
/// For tools that take no arguments, use an empty `args {}`:
///
/// ```ignore
/// define_tool!(pub(crate) struct Disconnect, args = DisconnectArgs,
///     tool_name: "disconnect",
///     desc: "Close SSH connection…",
///     params: json!({ "type": "object", "properties": {} }),
///     args {},
///     |this, _args| { … }
/// );
/// ```
///
/// In the body, `this` (name chosen by caller) refers to `&self` of the tool
/// struct, giving access to `this.state` ([`SessionState`]).
macro_rules! define_tool {
    // ── with args ──────────────────────────────────────────────
    (
        $vis:vis struct $name:ident, args = $args_name:ident,
        tool_name: $tool_name:literal,
        desc: $desc:literal,
        params: $params:expr,
        args { $($arg_field:ident: $arg_type:ty),* $(,)? },
        |$this:ident, $args:ident| $body:expr
    ) => {
        #[derive(serde::Deserialize)]
        $vis struct $args_name {
            $(pub $arg_field: $arg_type),*
        }

        $vis struct $name {
            #[allow(dead_code)]
            state: Arc<SessionState>,
        }

        impl $name {
            pub fn new(state: Arc<SessionState>) -> Self {
                Self { state }
            }
        }

        impl rig::tool::Tool for $name {
            const NAME: &'static str = $tool_name;

            type Error = rig::tool::ToolError;
            type Args = $args_name;
            type Output = String;

            async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
                rig::completion::ToolDefinition {
                    name: $tool_name.into(),
                    description: $desc.into(),
                    parameters: $params,
                }
            }

             #[allow(unused_variables)]
            async fn call(&self, $args: $args_name) -> Result<String, rig::tool::ToolError> {
                let $this = self;
                $body
            }
        }
    };

}

// Re-export so sub-modules can use `define_tool!` without importing it.
pub(crate) use define_tool;

// ── tool list builder ─────────────────────────────────────────────

/// Build the complete tool list based on [`Config::enabled_tool_groups`].
pub(crate) fn build_tools(config: &Config, state: &Arc<SessionState>) -> Vec<Box<dyn ToolDyn>> {
    let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();

    let s = || Arc::clone(state);

    if config.has_tool_group(ToolGroup::FileOps) {
        tools.push(Box::new(file::Read::new(s())));
        tools.push(Box::new(file::Write::new(s())));
        tools.push(Box::new(file::Replace::new(s())));
        tools.push(Box::new(file::ReadHtml::new(s())));
        tools.push(Box::new(file::Cd::new(s())));
    }
    if config.has_tool_group(ToolGroup::Shell) {
        tools.push(Box::new(shell::Shell::new(s())));
        if crate::session_state::is_goop_project_dir(&std::env::current_dir().unwrap_or_default()) {
            tools.push(Box::new(restart::Restart::new(s())));
        }
    }
    if config.has_tool_group(ToolGroup::Ssh) {
        tools.push(Box::new(ssh_tool::Ssh::new(s())));
        tools.push(Box::new(ssh_tool::Disconnect::new(s())));
    }
    if config.has_tool_group(ToolGroup::WebFetch) {
        tools.push(Box::new(web::WebFetch::new(s())));
    }
    if config.has_tool_group(ToolGroup::ComputerUse) {
        tools.push(Box::new(computer::Screenshot::new(s())));
        tools.push(Box::new(computer::CursorPosition::new(s())));
        tools.push(Box::new(computer::MouseMove::new(s())));
        tools.push(Box::new(computer::MouseClick::new(s())));
        tools.push(Box::new(computer::KeyType::new(s())));
        tools.push(Box::new(computer::KeyPress::new(s())));
        tools.push(Box::new(computer::WindowList::new(s())));
        tools.push(Box::new(computer::WindowFocus::new(s())));
        tools.push(Box::new(computer::WindowGetActive::new(s())));
        tools.push(Box::new(computer::OpenUrl::new(s())));
    }

    tools
}
