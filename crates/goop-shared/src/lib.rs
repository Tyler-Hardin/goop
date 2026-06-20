use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which view submitted this prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PromptSource {
    Terminal,
    Web,
}

/// Why a turn ended.  Every exit path in `Session::run_one` maps to exactly
/// one variant.  The reason is not just an audit label — it is functionally
/// necessary for correct replay: a `UserPrompt` whose turn ends with
/// `Cancelled { prompt: Some(_) }` is dropped from the agent-visible set
/// (the user cancelled before any work was committed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnEndReason {
    /// Agent produced a final response naturally.
    Completed,

    /// Stream ended without a FinalResponse item (unexpected).  Previously
    /// misrecorded as a clean completion — this variant distinguishes the two.
    StreamEnded,

    /// User cancelled the turn.
    ///
    /// `Some` → no work committed; the terminal repopulates its input for
    ///   editing; the entire turn (prompt + partial content) is NOT
    ///   agent-visible.
    /// `None` → work committed; the turn's content IS agent-visible.
    Cancelled { prompt: Option<String> },

    /// Max tool-calling turns exceeded.  Committed work is agent-visible.
    MaxTurnsExceeded { max_turns: usize },

    /// A stream or tool error occurred.  Committed work may be agent-visible.
    Error { message: String },
}

impl TurnEndReason {
    /// A user-facing error message for error-like reasons, or `None` for
    /// non-error reasons (which have their own UI handling).
    ///
    /// `MaxTurnsExceeded` and `Error` carry an actionable message; the
    /// other variants are handled specially by each view.
    pub fn error_message(&self) -> Option<String> {
        match self {
            TurnEndReason::MaxTurnsExceeded { max_turns } => Some(format!(
                "Reached the maximum number of tool-calling turns ({max_turns}). \
                 The work completed so far has been saved — send another message \
                 to continue."
            )),
            TurnEndReason::Error { message } => Some(message.clone()),
            _ => None,
        }
    }

    /// Short label for push-notification payloads.
    pub fn push_label(&self) -> &'static str {
        match self {
            TurnEndReason::Completed | TurnEndReason::StreamEnded => "Completed",
            TurnEndReason::Cancelled { .. } => "Cancelled",
            TurnEndReason::MaxTurnsExceeded { .. } => "MaxTurnsExceeded",
            TurnEndReason::Error { .. } => "Error",
        }
    }
}

/// Replacement content for an [`SessionEvent::Edited`] overlay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EditContent {
    /// Replaces a `UserPrompt` or `AssistantText` event's text.
    Text(String),
    /// Replaces a `ToolCall` event.
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    /// Replaces a `ToolResult` event.
    ToolResult { content: String },
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

    /// The assistant requested a tool call.  `id` pairs it with the
    /// matching [`ToolResult`](Self::ToolResult).
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },

    /// A tool result was received.  `id` matches the originating
    /// [`ToolCall`](Self::ToolCall).
    ToolResult { id: String, content: String },

    /// Estimated context window usage, emitted after each turn completes so
    /// the UI can show a progress bar.  `used` is an approximate token count
    /// of the conversation memory; `limit` is the context window (or
    /// compaction budget) it's measured against.
    ContextUsage { used: usize, limit: usize },

    /// Marks the end of a turn.  Every `run_one` invocation appends exactly
    /// one.  Replaces the former `FinalResponse` / `Error` / `Cancelled`
    /// trio with a single structured variant — see [`TurnEndReason`].
    TurnEnded { reason: TurnEndReason },

    /// A set of agent-visible events has been summarized into `summary`.
    /// Replaces those events (`covers`) in the agent's view.  In the UI,
    /// the covered events form a collapsible group.
    ///
    /// `covers` references the seqs of the **current agent-visible items**
    /// being replaced — including prior `Compacted`/`ToolSummarized`
    /// events — so overlapping/nested compactions are correct with no
    /// special cases.
    Compacted {
        summary: String,
        model: String,
        covers: Vec<u64>,
        manual: bool,
    },

    /// A single tool call+result pair has been summarized.  `id` matches the
    /// `ToolCall`/`ToolResult` it replaces.
    ToolSummarized {
        id: String,
        summary: String,
        model: String,
    },

    /// Recorded before each LLM call.  Lists the seqs of events that are
    /// agent-visible at this point (post-compaction, post-overlay), plus
    /// the model that is about to see them.  The log + these seqs + the
    /// model fully determine the messages the LLM received.
    ContextSnapshot { seqs: Vec<u64>, model: String },

    /// The session's active model changed mid-conversation.  Metadata only —
    /// does not change replay visibility.  Lets the UI annotate "model
    /// switched from X to Y here".
    ModelChanged { from: String, to: String },

    /// Replace the content of a prior event (`target` seq).  The original
    /// stays in the log; replay uses the replacement for the agent view.
    /// This is "writing into the LLM's mind" — the edited content is what
    /// the LLM sees on its next call.
    Edited {
        target: u64,
        replacement: EditContent,
    },

    /// Hide a prior event (`target` seq) from the agent's view.  Original
    /// preserved; replay skips it.
    Deleted { target: u64 },

    /// Sent by the server to a **single** subscriber after all history
    /// events have been replayed.  Not persisted to disk or emitted to
    /// other subscribers.  The client uses this to switch from bulk
    /// catch-up mode to live-event mode — all prior events are flushed
    /// into the message list in one batch instead of one by one.
    HistoryComplete,
}

/// One line in the append-only transaction log (JSONL).
///
/// The envelope carries tree structure (`parent`) and ordering (`seq`,
/// `ts`), wrapping the event payload.  This separation means replay walks
/// parent pointers (tree-aware), while the payload stays clean.
///
/// `parent` is currently always `Some(seq - 1)` (linear); the tree-walk
/// replay logic is implemented from the start so forking lands later with
/// no format migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Monotonic sequence number, assigned at append time.
    pub seq: u64,
    /// Parent event in the conversation tree.
    /// `None` = root; `Some(seq - 1)` = linear continuation; `Some(other)` = fork.
    pub parent: Option<u64>,
    /// When this entry was appended (UTC).  Enables UI features like
    /// relative timestamps, tool-call duration, and idle-gap display.
    pub ts: DateTime<Utc>,
    /// The actual event payload.
    pub event: SessionEvent,
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
    /// Replace the content of a prior event (`target` seq) in the agent's
    /// view.  The original stays in the log; replay uses the replacement.
    /// See [`SessionEvent::Edited`] and [`EditContent`].
    #[serde(rename = "edit")]
    Edit {
        target: u64,
        replacement: EditContent,
    },
    /// Hide a prior event (`target` seq) from the agent's view.  If the
    /// target is one half of a tool call+result pair, the server also
    /// deletes the matching half so the agent never sees an orphaned call
    /// or result.  See [`SessionEvent::Deleted`].
    #[serde(rename = "delete")]
    Delete { target: u64 },
}
