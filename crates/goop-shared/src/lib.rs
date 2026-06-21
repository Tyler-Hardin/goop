use std::collections::{HashMap, HashSet};

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

// ── config types shared between server and web UI ──────────────────────

/// Groups of tools that can be enabled/disabled in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    /// `read`, `write`, `replace`, `read_html`, `cd`
    FileOps,
    /// `shell`, `restart`
    Shell,
    /// `ssh`, `disconnect`
    Ssh,
    /// `screenshot`, `cursor_position`, `mouse_*`, `key_*`, `window_*`, `open_url`
    ComputerUse,
    /// `web_fetch`
    WebFetch,
}

/// Compaction budget for the conversation memory.
///
/// When the agent-visible conversation exceeds this budget, the entire prefix
/// is summarized by an LLM into a rolling summary before the next turn.
/// `None` disables compaction (unlimited context).
///
/// In config files this accepts either a bare integer (absolute tokens)
/// or a string like `"80%"` (percentage of the model's context window).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum CompactionMode {
    /// Absolute token budget.
    Tokens(usize),
    /// Percentage of the model's context window (0–100).
    Percent(u8),
}

impl<'de> Deserialize<'de> for CompactionMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = CompactionMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an integer (absolute tokens) or a string like \"80%\"")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(CompactionMode::Tokens(v as usize))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(CompactionMode::Tokens(v as usize))
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                if let Some(pct_str) = s.strip_suffix('%') {
                    let pct: u8 = pct_str
                        .trim()
                        .parse()
                        .map_err(|_| E::custom(format_args!("invalid percentage: {s:?}")))?;
                    if pct > 100 {
                        return Err(E::custom(format_args!(
                            "percentage out of range 0–100: {pct}"
                        )));
                    }
                    return Ok(CompactionMode::Percent(pct));
                }
                if let Ok(n) = s.trim().parse::<usize>() {
                    return Ok(CompactionMode::Tokens(n));
                }
                Err(E::custom(format_args!(
                    "expected integer or percentage string like \"80%\", got {s:?}"
                )))
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// Tool-pair summarization configuration.
///
/// When enabled, verbose tool call+result pairs are individually summarized
/// by an LLM, replacing the original call and result with a short summary.
/// This reclaims tokens incrementally without a full context compaction.
///
/// Independent of the full-compaction budget (`compaction`) — can be enabled
/// while full compaction is off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolSummarizationConfig {
    /// Whether tool-pair summarization is active.  Default: `false` (opt-in).
    #[serde(default)]
    pub enabled: bool,

    /// Model in `provider/model` format for summarization.  If `None`, uses
    /// the session's main model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Only summarize a pair when its call+result exceeds this many tokens.
    /// If `None`, a built-in default is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tokens: Option<usize>,

    /// Start summarizing when the agent-visible tool-call count exceeds this.
    /// If `None`, a built-in default is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_tool_count: Option<usize>,
}

/// Per-session overrides for the global config.  All fields are optional —
/// `None` means "defer to the global config".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionConfig {
    /// Override the model (provider/model format).  `None` = defer to global.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_max_turns: Option<usize>,
    /// Override the compaction budget.  `None` = defer to global.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_tool_groups: Option<Vec<ToolGroup>>,
    /// Override the Ollama base URL.  `None` = defer to global.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ollama_base_url: Option<String>,
    /// Names of MCP servers to enable for this session (adds to the
    /// global list — no need to repeat globally-enabled names here).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_mcp_servers: Option<Vec<String>>,
    /// Override tool-pair summarization.  `None` = defer to global.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_summarization: Option<ToolSummarizationConfig>,
}

// ── settings delta wire format ────────────────────────────────────────
//
// `SettingsUpdate` is what the client sends when the user changes a setting.
// Each field is `Option<Setting<T>>`:
//   - `None` (absent key in JSON)  → don't touch this field
//   - `Some(Setting::Set(v))`       → set override to `v`
//   - `Some(Setting::Clear)`        → remove override, go back to inheriting
//
// In JSON:  `{"model": "openai/gpt-4o"}` sets; `{"model": null}` clears.
// This is distinct from `SessionConfig` (the canonical override state),
// where `Option<T>` means "None = inherit, Some = override."

/// A setting change for a single field: set it, clear the override, or
/// (when wrapped in `Option`) leave it alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting<T> {
    /// Set the override to this value.
    Set(T),
    /// Remove the override — go back to inheriting from the global config.
    Clear,
}

impl<T: Serialize> Serialize for Setting<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Setting::Set(v) => v.serialize(s),
            Setting::Clear => s.serialize_unit(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Setting<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Visitor;
        use std::fmt;
        use std::marker::PhantomData;

        struct SettingVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for SettingVisitor<T> {
            type Value = Setting<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a value or null")
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Setting::Clear)
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Setting::Clear)
            }

            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<Self::Value, D2::Error> {
                T::deserialize(d).map(Setting::Set)
            }
        }

        d.deserialize_option(SettingVisitor(PhantomData))
    }
}

/// A settings change from the client.  Fields map to [`SessionConfig`] but
/// use [`Setting<T>`] so the user can explicitly clear an override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SettingsUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Setting<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama_base_url: Option<Setting<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tool_groups: Option<Setting<Vec<ToolGroup>>>,
}

// ── events ──────────────────────────────────────────────────────────
/// Views (terminal, web, phone, …) subscribe and render in their own way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionEvent {
    /// Session metadata — sent first on connect so clients know the
    /// session name for copy/paste at exit.
    ///
    /// `model` is the active model at session creation time.  It is
    /// `None` for legacy sessions created before this field was added;
    /// clients should track subsequent changes via
    /// [`SettingsChanged`](Self::SettingsChanged).
    SessionInfo {
        name: String,
        /// The active model when the session was created (provider/model format).
        /// `None` for legacy sessions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    /// The system prompt (preamble) the agent received at session creation.
    /// Appended to the log once (for new sessions) and persisted; on resume
    /// the stored value is authoritative — the preamble is NOT rebuilt.
    ///
    /// This is metadata: skipped during agent-memory replay (the preamble is
    /// already baked into the agent, not part of the conversation messages),
    /// but recorded in the log so it is a complete audit trail of what the
    /// LLM saw. The web UI's LLM view (👁) renders it above the message log.
    SystemPrompt { content: String },

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

    /// The session's settings changed mid-conversation.  Metadata only —
    /// does not change replay visibility.  Carries the complete set of
    /// session-level config overrides at this point so clients can track
    /// the active model, tool groups, etc. without field-by-field events.
    ///
    /// On resume, the server scans the log for the last `SettingsChanged`
    /// event; if the persisted session config differs, a new one is
    /// appended to bridge the gap.
    SettingsChanged { config: SessionConfig },

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

/// One item the server streams to a client over WebSocket.
///
/// Real conversation events arrive as [`Entry(LogEntry)`](ServerMessage::Entry),
/// carrying the full envelope (`seq`, `parent`, `ts`, `event`).  Sending the
/// envelope — not just the bare [`SessionEvent`] — gives the client the real
/// `seq` of every event (so overlay/compaction targeting works even on a
/// forked branch whose seqs are non-contiguous) and the `parent` (the
/// conversation-tree edge, used for branching).
///
/// [`HistoryComplete`](ServerMessage::HistoryComplete) is a sentinel injected
/// by the [`SessionSubscriber`](goop_server::SessionSubscriber) marking the end
/// of a history-replay batch — the initial catch-up *or* a post-fork
/// re-replay.  It is never appended to the log.
///
/// [`Reset`](ServerMessage::Reset) is broadcast when a fork happens.  Each
/// subscriber re-snapshots the active branch up to `tip` and re-replays it;
/// clients clear their state and re-enter catch-up.  Also never appended to
/// the log (a live-only signal).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ServerMessage {
    Entry(LogEntry),
    HistoryComplete,
    /// A fork happened at `tip` (the fork point — the parent of the new
    /// branch's first entry).  Subscribers re-replay the active branch up to
    /// `tip`; clients clear and re-catch-up.
    Reset {
        tip: u64,
    },
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
    /// Fork the conversation from the point *before* `target` (i.e. from
    /// `target`'s parent) and regenerate: a new `UserPrompt` carrying
    /// `content` is appended with `parent` set to that fork point, the
    /// active tip moves to the new branch, and a turn runs.  The old branch
    /// is preserved in the append-only log.  See §2.9 of the redesign doc.
    #[serde(rename = "fork")]
    Fork { target: u64, content: String },
    /// Manually compact a range of agent-visible messages into a summary.
    /// `covers` is the seqs of the messages to summarize.  The server
    /// collects those messages, calls LLM summarization, and appends a
    /// `Compacted` event with `manual = true`.  See §2.11 of the redesign
    /// doc.
    #[serde(rename = "compact_range")]
    CompactRange { covers: Vec<u64> },
    /// Update session settings mid-conversation.  Each field can be set to a
    /// new value, cleared (revert to global default), or left alone.  The
    /// server merges the changes, persists, appends `SettingsChanged`, and
    /// rebuilds the agent if needed.
    #[serde(rename = "update_settings")]
    UpdateSettings { config: SettingsUpdate },
}

// ── agent-visible projection ───────────────────────────────────────
//
// The transaction log records *what happened*; `build_agent_view` derives
// *what the LLM sees*.  This is the single source of truth for that
// projection — both the server and the web UI consume it.

/// One item in the agent-visible conversation, derived from the transaction
/// log by [`build_agent_view`].
///
/// This is the **single shared representation** of "what the LLM sees."
/// Both the server (which maps these to rig `Message`s, merging consecutive
/// items) and the web UI (which wraps them in reactive `UiMessage` variants)
/// consume this projection.
///
/// All changes to how the log is interpreted — compaction, tool
/// summarization, edit/delete, turn buffering, orphan cleanup — must be
/// made in [`build_agent_view`], not in the consumers.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentVisibleItem {
    /// A user prompt.
    UserText { seq: u64, content: String },
    /// A chunk of assistant text.
    AssistantText { seq: u64, content: String },
    /// A tool call.
    ToolCall {
        seq: u64,
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// A tool result.  Paired with its [`ToolCall`](AgentVisibleItem::ToolCall)
    /// by `id`.
    ToolResult { seq: u64, id: String, content: String },
    /// A compaction or tool-pair summary that replaced earlier items.  At the
    /// LLM API level, summaries are user-role text.
    Summary { seq: u64, content: String },
}

impl AgentVisibleItem {
    /// The transaction-log seq of the originating event.
    pub fn seq(&self) -> u64 {
        match self {
            AgentVisibleItem::UserText { seq, .. }
            | AgentVisibleItem::AssistantText { seq, .. }
            | AgentVisibleItem::ToolCall { seq, .. }
            | AgentVisibleItem::ToolResult { seq, .. }
            | AgentVisibleItem::Summary { seq, .. } => *seq,
        }
    }
}

/// Walk the conversation tree backward from `active_tip` to the root,
/// returning the active branch in chronological (root→tip) order.
///
/// `active_tip = None` means "the last entry" (the linear default) — every
/// entry is on the branch, so the whole log is returned in order.  `Some(tip)`
/// follows `parent` pointers from `tip` to the root, collecting ancestors;
/// entries not on that chain (sibling branches) are excluded.
///
/// Returns owned entries (cloned) so the result can outlive the log lock.
pub fn collect_branch(log: &[LogEntry], active_tip: Option<u64>) -> Vec<LogEntry> {
    let Some(tip) = active_tip.or_else(|| log.last().map(|e| e.seq)) else {
        return Vec::new();
    };
    let by_seq: HashMap<u64, &LogEntry> =
        log.iter().map(|e| (e.seq, e)).collect();
    let mut branch: Vec<&LogEntry> = Vec::new();
    let mut cur = Some(tip);
    while let Some(seq) = cur {
        let Some(entry) = by_seq.get(&seq) else {
            break;
        };
        cur = entry.parent;
        branch.push(entry);
    }
    branch.reverse();
    branch.into_iter().cloned().collect()
}

/// Build the agent-visible conversation from the transaction log.
///
/// This is the **single source of truth** for projecting the log into
/// "what the LLM sees."  Both the server (via `replay_visible`) and the
/// web UI (via `build_messages`) consume this function.
///
/// Applies, in order:
/// 1. **Branch selection** — walks `active_tip` to collect the active branch
/// 2. **Turn buffering** — accumulates content events until `TurnEnded`
/// 3. **Turn commit/drop** — `Cancelled { prompt: Some }` drops the turn
/// 4. **Compaction** — removes covered items, inserts a `Summary`
/// 5. **Tool summarization** — replaces a call+result pair with a `Summary`
/// 6. **Edit/Delete** — modifies or removes items
/// 7. **Orphan cleanup** — drops unpaired tool calls/results
///
/// The output is a flat list of **individual items** — consecutive items are
/// not merged.  The server wrapper merges consecutive items of the same role
/// for provider compatibility.  The web UI wraps them individually in
/// reactive signals.
///
/// **⚠️ Important:** all changes to how the agent's view is derived from the
/// log must be made here, not in the consumers.  The server-side
/// `replay_visible` and the web UI's `build_messages` are thin wrappers that
/// must not duplicate interpretation logic.
pub fn build_agent_view(
    entries: &[LogEntry],
    active_tip: Option<u64>,
) -> Vec<AgentVisibleItem> {
    let branch = collect_branch(entries, active_tip);
    build_agent_view_from_branch(&branch)
}

/// Core replay on a pre-collected branch.
fn build_agent_view_from_branch(branch: &[LogEntry]) -> Vec<AgentVisibleItem> {
    let mut visible: Vec<AgentVisibleItem> = Vec::new();
    let mut turn: Vec<AgentVisibleItem> = Vec::new();
    // Pending assistant text chunks (multiple `AssistantText` events before a
    // `ToolCall` or `TurnEnded`).  We track the seq of the first chunk so the
    // item carries the right seq for overlay/compaction targeting.
    let mut pending_text: Vec<(u64, String)> = Vec::new();

    /// Flush pending assistant text into the turn buffer as individual
    /// `AssistantText` items.
    fn flush_pending_text(turn: &mut Vec<AgentVisibleItem>, pending: &mut Vec<(u64, String)>) {
        for (seq, text) in pending.drain(..) {
            turn.push(AgentVisibleItem::AssistantText {
                seq,
                content: text,
            });
        }
    }

    for entry in branch {
        match &entry.event {
            SessionEvent::TurnEnded { reason } => {
                flush_pending_text(&mut turn, &mut pending_text);
                match reason {
                    TurnEndReason::Cancelled { prompt: Some(_) } => {
                        // No work committed — discard the whole turn.
                        turn.clear();
                    }
                    // Completed / StreamEnded / Cancelled { None } /
                    // MaxTurnsExceeded / Error — the turn's work is
                    // agent-visible.
                    _ => visible.append(&mut turn),
                }
            }

            // A compaction replaces a range of agent-visible items with a
            // rolling summary.  `covers` references the seqs of the *current*
            // visible items (including prior summaries), so a simple `retain`
            // is correct even for nested compactions.
            SessionEvent::Compacted {
                summary, covers, ..
            } => {
                let cover_set: HashSet<u64> = covers.iter().copied().collect();
                visible.retain(|i| !cover_set.contains(&i.seq()));
                visible.push(AgentVisibleItem::Summary {
                    seq: entry.seq,
                    content: summary.clone(),
                });
            }

            // A single tool call+result pair has been summarized.  Replaces
            // the pair (targeted by `id`) with the summary.
            SessionEvent::ToolSummarized { id, summary, .. } => {
                // Check whether the id exists at all — if both halves are
                // already gone (e.g. swept by a prior compaction), this is
                // a no-op.
                let has_call = visible.iter().any(|i| {
                    matches!(i, AgentVisibleItem::ToolCall { id: call_id, .. } if call_id == id)
                });
                let has_result = visible.iter().any(|i| {
                    matches!(i, AgentVisibleItem::ToolResult { id: result_id, .. } if result_id == id)
                });
                if !has_call && !has_result {
                    continue;
                }

                // Snapshot positions before removal so we know where to
                // insert the summary.
                let call_pos = visible.iter().position(|i| {
                    matches!(i, AgentVisibleItem::ToolCall { id: call_id, .. } if call_id == id)
                });
                // Only used as a fallback; the result is normally after the
                // call, so `call_pos` is the right insertion site.
                let result_pos = visible.iter().position(|i| {
                    matches!(i, AgentVisibleItem::ToolResult { id: result_id, .. } if result_id == id)
                });

                // Remove both call and result.
                visible.retain(|i| match i {
                    AgentVisibleItem::ToolCall { id: call_id, .. } => call_id != id,
                    AgentVisibleItem::ToolResult { id: result_id, .. } => result_id != id,
                    _ => true,
                });

                let summary_item = AgentVisibleItem::Summary {
                    seq: entry.seq,
                    content: summary.clone(),
                };
                // Insert at the call's position; fall back to the result's
                // position if the call was already gone (shouldn't normally
                // happen).  Both positions are from before `retain` and
                // remain valid because only the call and result (both at or
                // after these positions) were removed.
                match call_pos.or(result_pos) {
                    Some(pos) => visible.insert(pos, summary_item),
                    None => visible.push(summary_item),
                }
            }

            // ── overlay events ──
            SessionEvent::Edited {
                target,
                replacement,
            } => {
                apply_edit_agent(&mut visible, branch, *target, replacement);
            }
            SessionEvent::Deleted { target } => {
                apply_delete_agent(&mut visible, branch, *target);
            }

            // `SystemPrompt`, `ContextSnapshot`, `SettingsChanged`, `HistoryComplete`,
            // `SessionInfo`, `SessionState`, `ContextUsage` — metadata, not
            // conversation content.  Fall through to the turn buffer (they
            // become no-ops there).
            _ => feed_into_turn(&mut turn, &mut pending_text, entry),
        }
    }

    // NOTE: deliberately do NOT commit a trailing un-terminated turn — the
    // LLM appends the current prompt itself, so including the open turn
    // would duplicate the prompt.

    drop_orphaned_tool_pairs_agent(&mut visible);

    visible
}

/// Feed a log entry into the current turn buffer.
fn feed_into_turn(
    turn: &mut Vec<AgentVisibleItem>,
    pending_text: &mut Vec<(u64, String)>,
    entry: &LogEntry,
) {
    match &entry.event {
        SessionEvent::UserPrompt { content, .. } => {
            turn.push(AgentVisibleItem::UserText {
                seq: entry.seq,
                content: content.clone(),
            });
        }
        SessionEvent::AssistantText(text) => {
            // Accumulate consecutive text chunks.  Each chunk becomes its own
            // item (the server wrapper merges them later for provider compat).
            pending_text.push((entry.seq, text.clone()));
        }
        SessionEvent::ToolCall {
            id,
            name,
            arguments,
        } => {
            // Flush pending text first — tool calls are assistant-role,
            // and we want text chunks before them to be separate items.
            flush_pending_text_inline(turn, pending_text);
            turn.push(AgentVisibleItem::ToolCall {
                seq: entry.seq,
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
        }
        SessionEvent::ToolResult { id, content } => {
            flush_pending_text_inline(turn, pending_text);
            turn.push(AgentVisibleItem::ToolResult {
                seq: entry.seq,
                id: id.clone(),
                content: content.clone(),
            });
        }
        // Metadata / control events do not contribute messages.
        _ => {}
    }
}

#[inline]
fn flush_pending_text_inline(
    turn: &mut Vec<AgentVisibleItem>,
    pending: &mut Vec<(u64, String)>,
) {
    for (seq, text) in pending.drain(..) {
        turn.push(AgentVisibleItem::AssistantText {
            seq,
            content: text,
        });
    }
}

// ── orphan cleanup ─────────────────────────────────────────────────

/// Defence in depth: drop any `ToolCall` whose `ToolResult` is absent (or
/// vice-versa).  Catches in-flight tool calls from cancelled-with-work turns
/// and imperfect `Deleted` overlays.
fn drop_orphaned_tool_pairs_agent(visible: &mut Vec<AgentVisibleItem>) {
    let mut call_ids: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();
    for item in visible.iter() {
        match item {
            AgentVisibleItem::ToolCall { id, .. } => {
                call_ids.insert(id.clone());
            }
            AgentVisibleItem::ToolResult { id, .. } => {
                result_ids.insert(id.clone());
            }
            _ => {}
        }
    }

    let orphan_calls: HashSet<&str> = call_ids
        .iter()
        .map(|s| s.as_str())
        .filter(|id| !result_ids.contains(*id))
        .collect();
    let orphan_results: HashSet<&str> = result_ids
        .iter()
        .map(|s| s.as_str())
        .filter(|id| !call_ids.contains(*id))
        .collect();

    if orphan_calls.is_empty() && orphan_results.is_empty() {
        return;
    }

    visible.retain(|item| match item {
        AgentVisibleItem::ToolCall { id, .. } => !orphan_calls.contains(id.as_str()),
        AgentVisibleItem::ToolResult { id, .. } => !orphan_results.contains(id.as_str()),
        _ => true,
    });
}

// ── edit/delete overlay application ─────────────────────────────────
//
// These operate on the committed agent-visible set (after turn commit).
// Tool calls and results are targeted by seq — we look up the event payload
// in the log to get the tool-call id, then operate by id (since multiple
// items can share a seq after merging… but in the agent view items are
// individual, so seq match also works for non-tool targets).

/// Find the event payload at `target` seq in the log.
fn log_event_at<'a>(log: &'a [LogEntry], target: u64) -> Option<&'a SessionEvent> {
    log.iter().find(|e| e.seq == target).map(|e| &e.event)
}

/// Apply a `Deleted` overlay: hide `target` from the agent-visible set.
fn apply_delete_agent(visible: &mut Vec<AgentVisibleItem>, log: &[LogEntry], target: u64) {
    let Some(event) = log_event_at(log, target) else {
        return; // target not in the log — no-op.
    };
    match event {
        SessionEvent::ToolCall { id, .. } => {
            visible.retain(|i| !matches!(i, AgentVisibleItem::ToolCall { id: call_id, .. } if call_id == id));
        }
        SessionEvent::ToolResult { id, .. } => {
            visible.retain(|i| !matches!(i, AgentVisibleItem::ToolResult { id: result_id, .. } if result_id == id));
        }
        _ => {
            visible.retain(|i| i.seq() != target);
        }
    }
}

/// Apply an `Edited` overlay: replace `target`'s content with `replacement`.
fn apply_edit_agent(
    visible: &mut [AgentVisibleItem],
    log: &[LogEntry],
    target: u64,
    replacement: &EditContent,
) {
    // Tool targets need the id from the log.
    if let Some(event) = log_event_at(log, target) {
        match (event, replacement) {
            (
                SessionEvent::ToolCall { id, .. },
                EditContent::ToolCall { name, arguments },
            ) => {
                for item in visible.iter_mut() {
                    if let AgentVisibleItem::ToolCall {
                        id: call_id,
                        name: item_name,
                        arguments: item_args,
                        ..
                    } = item
                        && call_id == id
                    {
                        *item_name = name.clone();
                        *item_args = arguments.clone();
                    }
                }
                return;
            }
            (SessionEvent::ToolResult { id, .. }, EditContent::ToolResult { content }) => {
                for item in visible.iter_mut() {
                    if let AgentVisibleItem::ToolResult {
                        id: result_id,
                        content: item_content,
                        ..
                    } = item
                        && result_id == id
                    {
                        *item_content = content.clone();
                    }
                }
                return;
            }
            _ => {}
        }
    }

    // Text replacement — operates on the item whose seq matches.
    let EditContent::Text(text) = replacement else {
        return;
    };
    let Some(item) = visible.iter_mut().find(|i| i.seq() == target) else {
        return;
    };
    let text = text.clone();
    match item {
        AgentVisibleItem::UserText { content, .. }
        | AgentVisibleItem::AssistantText { content, .. }
        | AgentVisibleItem::Summary { content, .. } => {
            *content = text;
        }
        // ToolCall/ToolResult edits are handled above.
        _ => {}
    }
}
