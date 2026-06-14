use serde::Serialize;

/// Which view submitted this prompt.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[allow(dead_code)]
pub enum PromptSource {
    Terminal,
    Web,
}

/// Events emitted by the session as the agent processes a prompt.
/// Views (terminal, web, phone, …) subscribe and render in their own way.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)] // some variants/fields are consumed by future views
#[serde(tag = "type", content = "data")]
pub enum SessionEvent {
    /// A user submitted a prompt.  Arrives *before* Thinking.
    UserPrompt {
        content: String,
        source: PromptSource,
    },

    /// The agent has started a new turn and is "thinking".
    Thinking,

    /// A chunk of assistant markdown text (may be partial/incomplete).
    AssistantText(String),

    /// The assistant requested a tool call.
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },

    /// A tool result was received.
    ToolResult { content: String },

    /// The assistant finished its complete response.
    FinalResponse,

    /// A recoverable or informational error.
    Error(String),

    /// The current prompt was cancelled by the user.
    Cancelled,
}
