use serde::{Deserialize, Serialize};

/// Which view submitted this prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PromptSource {
    Terminal,
    Web,
}

/// Events emitted by the session as the agent processes a prompt.
/// Views (terminal, web, phone, …) subscribe and render in their own way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionEvent {
    /// Session metadata — sent first on connect so clients know the
    /// session name for copy/paste at exit.
    SessionInfo { name: String },

    /// The session is currently processing a prompt (true) or idle (false).
    /// Sent to late-joining clients after history replay so they know
    /// whether to show a Cancel button.  Not persisted to disk.
    SessionState { running: bool },

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
    ///
    /// When `prompt` is set, no agent output was produced before the
    /// cancel — the terminal should repopulate its input so the user
    /// can edit and resubmit.  When `None`, completed tool turns were
    /// already saved to memory and the next prompt starts fresh.
    Cancelled { prompt: Option<String> },

    /// Sent by the server to a **single** subscriber after all history
    /// events have been replayed.  Not persisted to disk or emitted to
    /// other subscribers.  The client uses this to switch from bulk
    /// catch-up mode to live-event mode — all prior events are flushed
    /// into the message list in one batch instead of one by one.
    HistoryComplete,
}

/// Messages sent from client to server over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Submit a text prompt.
    #[serde(rename = "prompt")]
    Prompt { content: String },
    /// Cancel the current prompt.
    #[serde(rename = "cancel")]
    Cancel,
}
