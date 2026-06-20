use std::collections::HashSet;

use goop_shared::{
    ClientMessage, EditContent, LogEntry, ServerMessage, SessionEvent, TurnEndReason,
};
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::components::input_button::BtnState;
use crate::ws;

/// WebSocket connection lifecycle — replaces the former `connected` and
/// `catching_up` booleans with a single exhaustive enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// No WebSocket open.
    Disconnected,
    /// WebSocket connected, replaying history events.  UI should show a
    /// skeletal layout; no interactive prompts yet.
    CatchingUp,
    /// History replay complete — the session is fully interactive.
    Connected,
}

impl ConnectionState {
    /// Session is fully interactive — history replayed, ready for prompts.
    pub fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }
    pub fn is_catching_up(self) -> bool {
        matches!(self, Self::CatchingUp)
    }
    /// WebSocket transport is open at the TCP level (green dot).  True
    /// during both `CatchingUp` and `Connected` — only false when the
    /// socket is actually down.
    pub fn is_ws_open(self) -> bool {
        matches!(self, Self::Connected | Self::CatchingUp)
    }
}

// ── turn state machine ──────────────────────────────────────────────

/// Tracks where we are within a single prompt+response lifecycle.
///
/// The key invariant: a `UiMessage::Thinking` is present at the end of
/// `messages` **iff** the state is [`Thinking`].  Every transition out
/// of `Thinking` must remove that message; every transition into
/// `Thinking` must push one.  This replaces the former `thinking: bool` +
/// `remove_last_thinking()` pattern — two implicit representations of the
/// same fact that could (and did) diverge.
///
/// ```text
///                    ┌──────────────────────────────────┐
///                    │                                  │
///   ┌────┐  UserPrompt  ┌──────────┐  AssistantText  ┌──────────┐
///   │Idle│─────────────▶│ Thinking │────────────────▶│  Active  │
///   └──▲──┘              └────┬─────┘                 └────┬─────┘
///      │     TurnEnded       │                            │
///      │     (any reason)    │ ToolCall                   │ ToolCall
///      │                      │ TurnEnded                  │ TurnEnded
///      │                      ▼                            ▼
///      │                  ┌──────┐                    ┌──────┐
///      │                  │ Idle │◀───────────────────│ Idle │
///      │                  └──┬───┘                    └──┬───┘
///      │                     │                           │
///      │                     │ Thinking (inter-turn)     │ Thinking (inter-turn)
///      │                     ▼                           ▼
///      │                 ┌──────────┐                ┌──────────┐
///      └─────────────────│ Thinking │◀───────────────│ Thinking │
///                        └──────────┘                └──────────┘
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnState {
    /// No prompt in progress.  No Thinking message in the list.
    Idle,
    /// A `Thinking` event was received — a `UiMessage::Thinking` is at
    /// the end of `messages`.  The next content-bearing event must
    /// remove it before pushing its own message.
    Thinking,
    /// Turn is in progress (streaming text, tool executing, etc.) but
    /// no Thinking placeholder is present.
    Active,
}

impl TurnState {
    /// Whether a `UiMessage::Thinking` is currently in the message list.
    fn has_thinking_msg(self) -> bool {
        matches!(self, Self::Thinking)
    }
}

/// Edit-overlay state for a message whose content was replaced by an
/// [`SessionEvent::Edited`] event.  The original is preserved so the UI can
/// toggle between the edited and original views ("show original").
#[derive(Clone, Debug)]
pub(crate) struct EditOverlay {
    /// The replacement content (what the agent now sees), as a display string.
    pub replacement: String,
    /// `true` while the UI is showing the original instead of the replacement.
    pub show_original: RwSignal<bool>,
}

/// UI-facing message type.  Derived from `SessionEvent` by the dispatch
/// function — keeps raw event shapes out of the component tree.
///
/// Each message carries a unique `id` so `<For>` (keyed iteration) can
/// track individual messages across re-renders.  Without stable keys,
/// every `messages` signal update recreates all DOM nodes, retriggering
/// CSS animations (flash).
///
/// Agent-visible variants (those that correspond to something the LLM sees)
/// carry a `seq` — the transaction-log sequence number of the originating
/// event.  This lets later overlay events ([`SessionEvent::Edited`],
/// [`SessionEvent::Deleted`]) and compaction
/// ([`SessionEvent::Compacted`] `covers`) target them.  The seq comes
/// directly from the [`LogEntry`] envelope the server sends (see
/// [`ServerMessage::Entry`]) — not a counted value — so it stays correct on a
/// forked branch whose seqs are non-contiguous.
#[derive(Clone, Debug)]
pub enum UiMessage {
    UserPrompt {
        id: usize,
        seq: u64,
        content: String,
        deleted: RwSignal<bool>,
        edit: RwSignal<Option<EditOverlay>>,
    },
    Thinking {
        id: usize,
    },
    AssistantFinal {
        id: usize,
        seq: u64,
        raw: String,
        deleted: RwSignal<bool>,
        edit: RwSignal<Option<EditOverlay>>,
    },
    ToolCall {
        id: usize,
        seq: u64,
        /// Logical tool-call id (pairs the call with its `ToolResult` and is
        /// the target of [`ToolSummarized`](SessionEvent::ToolSummarized)).
        tool_id: String,
        name: String,
        args: Vec<(String, String)>,
        /// Populated when the corresponding `ToolResult` arrives.
        ///
        /// This is a signal (not a plain `Option<String>`) because the
        /// `ToolResult` event arrives *after* the `Message` component has
        /// already been rendered by `<For>`.  `<For>` keys items by `id`
        /// and does not re-run the child view for an unchanged key, so a
        /// by-value `result` field would never update in the DOM.  A signal
        /// updates the view reactively regardless of `<For>` reconciliation.
        result: RwSignal<Option<String>>,
        /// Seq of the `ToolResult` event, for overlay targeting.  Set when
        /// the result arrives.
        result_seq: RwSignal<Option<u64>>,
        expanded: RwSignal<bool>,
        deleted: RwSignal<bool>,
        /// Edit overlay for the call's name/args.
        edit: RwSignal<Option<EditOverlay>>,
        /// Edit overlay for the result content.
        result_edit: RwSignal<Option<EditOverlay>>,
    },
    FinalResponse {
        id: usize,
    },
    Error {
        id: usize,
        msg: String,
    },
    Cancelled {
        id: usize,
    },
    /// A rolling LLM summary that replaced earlier messages.  Collapsed by
    /// default (showing the summary); click to expand and see the originals.
    /// `children` may contain nested groups (recursive summaries) or
    /// [`ToolSummaryGroup`](UiMessage::ToolSummaryGroup)s.
    CompactedGroup {
        id: usize,
        seq: u64,
        summary: String,
        model: String,
        manual: bool,
        children: Vec<UiMessage>,
        expanded: RwSignal<bool>,
    },
    /// A single tool call+result pair summarized into a short summary.
    /// Collapsed by default; expand to see the original pair.
    ToolSummaryGroup {
        id: usize,
        seq: u64,
        summary: String,
        model: String,
        child: Box<UiMessage>,
        expanded: RwSignal<bool>,
    },
}

impl UiMessage {
    /// Return the message's unique ID for use as a `<For>` key.
    pub fn id(&self) -> usize {
        match self {
            UiMessage::UserPrompt { id, .. } => *id,
            UiMessage::Thinking { id } => *id,
            UiMessage::AssistantFinal { id, .. } => *id,
            UiMessage::ToolCall { id, .. } => *id,
            UiMessage::FinalResponse { id } => *id,
            UiMessage::Error { id, .. } => *id,
            UiMessage::Cancelled { id } => *id,
            UiMessage::CompactedGroup { id, .. } => *id,
            UiMessage::ToolSummaryGroup { id, .. } => *id,
        }
    }

    /// The transaction-log seq of the originating event, if this message is
    /// agent-visible (and thus targetable by overlays / compaction `covers`).
    pub fn agent_seq(&self) -> Option<u64> {
        match self {
            UiMessage::UserPrompt { seq, .. }
            | UiMessage::AssistantFinal { seq, .. }
            | UiMessage::ToolCall { seq, .. }
            | UiMessage::CompactedGroup { seq, .. }
            | UiMessage::ToolSummaryGroup { seq, .. } => Some(*seq),
            UiMessage::Thinking { .. }
            | UiMessage::FinalResponse { .. }
            | UiMessage::Error { .. }
            | UiMessage::Cancelled { .. } => None,
        }
    }

    /// Whether this message has been deleted (hidden from the agent's view
    /// by a `Deleted` overlay).  Used to filter messages in LLM view.
    pub fn is_deleted(&self) -> bool {
        match self {
            UiMessage::UserPrompt { deleted, .. }
            | UiMessage::AssistantFinal { deleted, .. }
            | UiMessage::ToolCall { deleted, .. } => deleted.get_untracked(),
            _ => false,
        }
    }
}

/// Global reactive state for the web UI.
#[derive(Clone)]
pub struct AppState {
    /// Currently selected session name (None = no session).
    pub current_session: RwSignal<Option<String>>,
    /// All session names known to the server.
    pub sessions: RwSignal<Vec<String>>,
    /// Messages rendered in the log.
    pub messages: RwSignal<Vec<UiMessage>>,
    /// True while the LLM is processing a prompt (derived from `btn_state`).
    pub running: Signal<bool>,
    /// Unified button state machine — owns the send/mic/cancel button.
    pub btn_state: RwSignal<BtnState>,
    /// WebSocket connection lifecycle.
    pub connection: RwSignal<ConnectionState>,
    /// Active `WebSocket` handle for sending messages.
    pub ws: RwSignal<Option<web_sys::WebSocket>>,
    /// Streaming text buffer — accumulated on each `AssistantText` event,
    /// flushed to a `UiMessage::AssistantFinal` on `TurnEnded`.
    pub streaming_text: RwSignal<String>,
    /// Seq of the first `AssistantText` chunk in the current stream.  The
    /// server merges consecutive `AssistantText` events into one
    /// agent-visible item whose seq is the *first* chunk's; we track it
    /// here so the flushed `AssistantFinal` carries the right seq.
    pub(crate) streaming_seq: RwSignal<Option<u64>>,
    /// Turn-level state machine.  When `Thinking`, a `UiMessage::Thinking`
    /// is present at the end of `messages` and the animated dot is shown.
    /// Replaces the former `thinking: bool` + `remove_last_thinking()`
    /// pattern.
    pub turn_state: RwSignal<TurnState>,
    /// Sidebar open state (for mobile slide-out).
    pub sidebar_open: RwSignal<bool>,
    /// Current text in the input area.
    pub input_text: RwSignal<String>,
    /// Monotonically increasing message ID counter.  Each `UiMessage` gets
    /// a unique ID so `<For>` can track messages by key.
    pub next_message_id: RwSignal<usize>,

    /// Estimated context window usage: `(used_tokens, limit_tokens)`.
    /// `None` until the first `ContextUsage` event arrives (or when the
    /// model's context window is unknown and compaction is disabled).
    /// Drives the thin progress bar at the top of the input footer.
    pub context_usage: RwSignal<Option<(usize, usize)>>,

    // ── history catch-up ──────────────────────────────────────────
    /// Raw `LogEntry` envelopes accumulated during history replay.  No signals
    /// are touched while `connection` is `CatchingUp` — every entry lands
    /// here.  On `HistoryComplete` the buffer is pre-formed into a single
    /// `Vec<UiMessage>` and all signals are set in one shot.
    pub(crate) history_buffer: RwSignal<Vec<LogEntry>>,

    /// Monotonically increasing connection counter.  Each `connect()`
    /// increments it; the `on_close` callback captures the value at
    /// creation time and skips cleanup if it no longer matches — this
    /// prevents a deferred `on_close` from a stale connection from
    /// clobbering the new connection's catch-up state.
    pub(crate) connection_gen: RwSignal<u64>,

    /// Monotonically increasing counter — incremented each time the
    /// input textarea should grab focus (e.g. after connecting to a
    /// new session).  The `InputBar` component watches this and calls
    /// `.focus()` whenever it changes.
    pub(crate) input_focus_request: RwSignal<usize>,

    /// Auto-reconnect backoff counter.  Incremented each time the
    /// WebSocket closes unexpectedly; reset to 0 on manual reconnect
    /// or successful open.  Used by [`schedule_reconnect`] for
    /// exponential backoff (1s, 2s, 4s, …, 64s cap).
    pub(crate) reconnect_attempt: RwSignal<u32>,

    /// Range-select mode for manual range compaction.  When `true`,
    /// clicking a message sets the start/end of a contiguous range to
    /// compact.  The input bar is replaced by a selection bar.  See §2.11
    /// of the redesign doc.
    pub select_mode: RwSignal<bool>,
    /// Index into `messages` of the range start (first click).
    pub selection_start: RwSignal<Option<usize>>,
    /// Index into `messages` of the range end (second click).  `None`
    /// means only the start is set (single message — not enough to
    /// compact).
    pub selection_end: RwSignal<Option<usize>>,

    /// "LLM view" — when `true`, the message log shows exactly what the
    /// agent sees: compaction summaries as plain messages (no tree nodes),
    /// deleted messages hidden, tool summaries flattened.  Toggled by the
    /// 👁 button in the header.
    pub llm_view: RwSignal<bool>,
}

/// Result of pre-forming buffered history entries into UI state.
struct BuildResult {
    messages: Vec<UiMessage>,
    session_name: Option<String>,
    running: bool,
    turn_state: TurnState,
    next_id: usize,
    context_usage: Option<(usize, usize)>,
}

impl AppState {
    pub fn new() -> Self {
        let btn_state = RwSignal::new(BtnState::Disabled);
        let running = Signal::derive(move || btn_state.get() == BtnState::Running);

        Self {
            current_session: RwSignal::new(None),
            sessions: RwSignal::new(Vec::new()),
            messages: RwSignal::new(Vec::new()),
            running,
            btn_state,
            connection: RwSignal::new(ConnectionState::Disconnected),
            ws: RwSignal::new(None),
            streaming_text: RwSignal::new(String::new()),
            streaming_seq: RwSignal::new(None),
            turn_state: RwSignal::new(TurnState::Idle),
            sidebar_open: RwSignal::new(false),
            input_text: RwSignal::new(String::new()),
            next_message_id: RwSignal::new(0),
            context_usage: RwSignal::new(None),
            history_buffer: RwSignal::new(Vec::new()),
            connection_gen: RwSignal::new(0),
            input_focus_request: RwSignal::new(0),
            reconnect_attempt: RwSignal::new(0),
            select_mode: RwSignal::new(false),
            selection_start: RwSignal::new(None),
            selection_end: RwSignal::new(None),
            llm_view: RwSignal::new(false),
        }
    }

    // ── WebSocket actions ──────────────────────────────────────────

    /// Connect (or reconnect) to the named session.
    pub fn connect_session(&self, name: String) {
        // Update URL hash so bookmarks work.
        if let Some(w) = web_sys::window() {
            let _ = w.location().set_hash(&format!("#session={name}"));
        }
        self.input_focus_request.update(|n| *n += 1);
        self.reconnect_attempt.set(0); // reset backoff on manual reconnect
        ws::connect(self, name);
    }

    /// Schedule an automatic reconnection attempt with exponential
    /// backoff.  Called from the WebSocket `on_close` handler when the
    /// connection drops unexpectedly (server restart, network loss).
    ///
    /// `conn_gen` is the generation at the time of the disconnect; if the
    /// generation has changed by the time the timer fires (e.g. a manual
    /// reconnect beat us to it), the attempt is silently skipped.
    pub(crate) fn schedule_reconnect(&self, conn_gen: u64) {
        let attempt = self.reconnect_attempt.get_untracked();
        // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, cap at 64s.
        let delay_ms = (1u64 << attempt.min(6)) * 1000;

        let state = self.clone();
        if let Some(window) = web_sys::window() {
            let cb = wasm_bindgen::prelude::Closure::once(move || {
                if state.connection_gen.get_untracked() != conn_gen {
                    // A manual reconnect (or a newer auto-reconnect) already
                    // changed the generation — nothing to do.
                    return;
                }
                let session = match state.current_session.get_untracked() {
                    Some(s) => s,
                    None => return,
                };
                log::info!("auto-reconnect attempt {attempt} to {session} (delay {delay_ms}ms)");
                // Use ws::connect directly — connect_session resets
                // reconnect_attempt, but we want the counter to keep
                // growing across auto-reconnect attempts until one
                // succeeds (at which point on_open resets it).
                ws::connect(&state, session);
                // If this connect fails, the new on_close will bump
                // reconnect_attempt and call schedule_reconnect again.
            });
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                delay_ms as i32,
            );
            cb.forget();
        }
    }

    /// Send a text prompt to the server.
    pub fn send_prompt(&self, content: String) {
        self.btn_state.set(BtnState::Running);
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            let msg = serde_json::to_string(&ClientMessage::Prompt { content }).unwrap_or_default();
            ws.send_with_str(&msg).ok();
        }
    }

    /// Cancel the current in-flight prompt.
    pub fn cancel(&self) {
        self.btn_state.set(BtnState::Idle);
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            let msg = serde_json::to_string(&ClientMessage::Cancel).unwrap_or_default();
            ws.send_with_str(&msg).ok();
        }
    }

    /// Hide a prior event (`target` seq) from the agent's view.  The server
    /// appends a `Deleted` overlay (and, for a tool call/result, one for the
    /// matching half) which comes back as a live event and is applied by the
    /// existing `apply_delete` path.
    pub fn delete_message(&self, target: u64) {
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            let msg = serde_json::to_string(&ClientMessage::Delete { target }).unwrap_or_default();
            ws.send_with_str(&msg).ok();
        }
    }

    /// Replace a prior event's (`target` seq) content in the agent's view.
    /// The server appends an `Edited` overlay which comes back as a live
    /// event and is applied by the existing `apply_edit` path (setting an
    /// `EditOverlay` so the UI shows the replacement with a ✎ toggle).
    pub fn edit_message(&self, target: u64, replacement: EditContent) {
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            let msg = serde_json::to_string(&ClientMessage::Edit {
                target,
                replacement,
            })
            .unwrap_or_default();
            ws.send_with_str(&msg).ok();
        }
    }

    /// Fork the conversation from the point before `target` (edit-and-
    /// regenerate).  The server appends a new `UserPrompt` with `content`
    /// branching from `target`'s parent, moves the active tip to the new
    /// branch, and runs a turn.  The client receives a `Reset` and re-catches-
    /// up to the new branch automatically.  See §2.9 of the redesign doc.
    pub fn fork_message(&self, target: u64, content: String) {
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            self.btn_state.set(BtnState::Running);
            let msg =
                serde_json::to_string(&ClientMessage::Fork { target, content }).unwrap_or_default();
            ws.send_with_str(&msg).ok();
        }
    }

    // ── manual range compaction (select mode) ───────────────────────

    /// Toggle select mode on/off.  Turning off clears the selection.
    pub fn toggle_select_mode(&self) {
        if self.select_mode.get_untracked() {
            self.exit_select_mode();
        } else {
            self.select_mode.set(true);
        }
    }

    /// Exit select mode and clear the selection.
    pub fn exit_select_mode(&self) {
        self.select_mode.set(false);
        self.selection_start.set(None);
        self.selection_end.set(None);
    }

    /// Click a message in select mode.  First click sets the range start;
    /// second click sets the end (everything between is selected).  A third
    /// click resets to a new start.  Clicking the same message twice
    /// deselects.
    pub fn select_click(&self, idx: usize) {
        let start = self.selection_start.get_untracked();
        let end = self.selection_end.get_untracked();
        match (start, end) {
            (None, _) => {
                // First click — set start.
                self.selection_start.set(Some(idx));
                self.selection_end.set(None);
            }
            (Some(s), None) if s == idx => {
                // Clicked the start again — deselect.
                self.selection_start.set(None);
            }
            (Some(s), None) => {
                // Second click — set end.  Ensure start <= end.
                self.selection_start.set(Some(s.min(idx)));
                self.selection_end.set(Some(s.max(idx)));
            }
            _ => {
                // Both already set — start a new selection.
                self.selection_start.set(Some(idx));
                self.selection_end.set(None);
            }
        }
    }

    /// Whether message at `idx` is inside the current selection range.
    ///
    /// Uses tracked `.get()` so reactive callers (e.g. `class:selected`
    /// closures) re-evaluate when the selection changes.
    pub fn is_in_selection(&self, idx: usize) -> bool {
        let start = self.selection_start.get();
        let end = self.selection_end.get();
        match (start, end) {
            (Some(s), Some(e)) => idx >= s && idx <= e,
            (Some(s), None) => idx == s,
            _ => false,
        }
    }

    /// Number of messages in the selection range (0 if no selection).
    ///
    /// Uses tracked `.get()` so the `SelectBar`'s derived count signal
    /// re-evaluates when the selection changes.
    pub fn selection_count(&self) -> usize {
        let start = self.selection_start.get();
        let end = self.selection_end.get();
        match (start, end) {
            (Some(s), Some(e)) => e - s + 1,
            (Some(_), None) => 1,
            _ => 0,
        }
    }

    /// Send a manual compaction request for the selected range, then exit
    /// select mode.  Collects the seqs of agent-visible messages in the
    /// range and sends them as `covers`.  The server summarizes them and
    /// appends a `Compacted { manual: true }` event.  See §2.11.
    ///
    /// **Important:** the selection indices come from the **displayed**
    /// message list, which may be filtered (LLM view hides deleted
    /// messages).  We must apply the same filter here so the indices line
    /// up — otherwise `covers` points at the wrong messages.
    pub fn compact_selected(&self) {
        let start = self.selection_start.get_untracked();
        let end = self.selection_end.get_untracked();
        let Some((s, e)) = start.zip(end) else {
            return;
        };
        let llm_view = self.llm_view.get_untracked();
        let covers: Vec<u64> = self
            .messages
            .get_untracked()
            .iter()
            .filter(|m| !llm_view || !m.is_deleted())
            .enumerate()
            .filter(|(i, _)| *i >= s && *i <= e)
            .filter_map(|(_, m)| m.agent_seq())
            .collect();
        if covers.len() < 2 {
            return;
        }
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            let msg =
                serde_json::to_string(&ClientMessage::CompactRange { covers }).unwrap_or_default();
            ws.send_with_str(&msg).ok();
        }
        self.exit_select_mode();
    }

    /// Send raw WAV audio to the server for speech-to-text transcription.
    pub fn send_audio(&self, data: Vec<u8>) {
        if let Some(ws) = self.ws.get_untracked()
            && ws.ready_state() == web_sys::WebSocket::OPEN
        {
            self.btn_state.set(BtnState::Running);
            ws.send_with_u8_array(&data).ok();
        }
    }

    /// Fetch the list of active sessions from the REST API.
    pub async fn fetch_sessions(&self) {
        let resp = gloo_net::http::Request::get("/api/sessions").send().await;
        match resp {
            Ok(r) => match r.json::<Vec<String>>().await {
                Ok(names) => {
                    log::info!("fetch_sessions got {} names: {:?}", names.len(), names);
                    self.sessions.set(names);
                }
                Err(e) => log::warn!("fetch_sessions JSON parse failed: {e}"),
            },
            Err(e) => log::warn!("failed to fetch sessions: {e}"),
        }
    }

    /// Create a new session via REST, returning its name.
    ///
    /// Also calls `fetch_sessions` on success so the sidebar updates.
    pub async fn create_session(&self, name: Option<String>) -> Option<String> {
        let body = match name {
            Some(n) => format!("{{\"name\":\"{}\"}}", n.replace('"', "\\\"")),
            None => "{}".to_string(),
        };
        let request = match gloo_net::http::Request::post("/api/sessions")
            .header("Content-Type", "application/json")
            .body(body)
        {
            Ok(req) => req,
            Err(e) => {
                log::warn!("failed to create session request: {e}");
                return None;
            }
        };
        let resp = request.send().await;
        match resp {
            Ok(r) => {
                // Read the session name from the response body.
                // Server returns `{"name": "20260618_001"}`.
                let created_name = match r.json::<serde_json::Value>().await {
                    Ok(json) => json
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    Err(e) => {
                        log::warn!("failed to parse create session response: {e}");
                        None
                    }
                };
                // Fetch the full list so the sidebar updates.
                self.fetch_sessions().await;
                created_name
            }
            Err(e) => {
                log::warn!("failed to create session: {e}");
                None
            }
        }
    }

    /// Close a session via REST DELETE, then refetch list.
    pub async fn delete_session(&self, name: &str) {
        let url = format!("/api/sessions/{name}");
        let resp = gloo_net::http::Request::delete(&url).send().await;
        if let Err(e) = resp {
            log::warn!("failed to delete session: {e}");
        }
        self.fetch_sessions().await;
    }

    // ── Event intake (called from ws.rs) ──────────────────────────

    /// Handle an incoming [`ServerMessage`] from the WebSocket.
    ///
    /// During history catch-up (`connection == CatchingUp`), [`Entry`](ServerMessage::Entry)
    /// envelopes are buffered without touching any reactive signals.  When
    /// [`HistoryComplete`](ServerMessage::HistoryComplete) arrives the entire
    /// buffer is pre-formed into a single `Vec<UiMessage>` and all signals are
    /// set in one shot — the page materialises instantly, as if the chat had
    /// already happened.
    ///
    /// A [`Reset`](ServerMessage::Reset) (fork) clears the UI and re-enters
    /// catch-up: the server re-replays the new active branch, which arrives as
    /// a fresh batch of `Entry` envelopes followed by another `HistoryComplete`.
    ///
    /// In live mode, each `Entry` is dispatched immediately.
    pub fn handle_subscribed(&self, msg: ServerMessage) {
        match msg {
            ServerMessage::Reset { .. } => {
                // A fork happened — clear state and re-enter catch-up.  The
                // server re-replays the new active branch next.
                self.messages.set(Vec::new());
                self.history_buffer.set(Vec::new());
                self.streaming_text.set(String::new());
                self.streaming_seq.set(None);
                self.turn_state.set(TurnState::Idle);
                self.context_usage.set(None);
                self.connection.set(ConnectionState::CatchingUp);
                self.exit_select_mode();
            }
            ServerMessage::HistoryComplete => {
                let buffer = self.history_buffer.get_untracked();
                self.history_buffer.set(Vec::new());
                self.connection.set(ConnectionState::Connected);

                // Pre-form all messages from the buffered entries
                // without touching a single reactive signal.
                let start_id = self.next_message_id.get_untracked();
                let result = Self::build_messages(&buffer, start_id);
                self.next_message_id.set(result.next_id);

                // Apply everything in one shot.
                if let Some(name) = result.session_name {
                    self.current_session.set(Some(name));
                }
                self.btn_state.set(if result.running {
                    BtnState::Running
                } else {
                    BtnState::Idle
                });
                self.turn_state.set(result.turn_state);
                self.streaming_text.set(String::new());
                self.messages.set(result.messages);
                self.context_usage.set(result.context_usage);

                // Session is now loaded — refresh the sidebar.
                log::info!("HistoryComplete — refreshing session list");
                let s = self.clone();
                leptos::task::spawn_local(async move {
                    s.fetch_sessions().await;
                });
            }
            ServerMessage::Entry(entry) => {
                if self.connection.get_untracked().is_catching_up() {
                    self.history_buffer.update(|buf| buf.push(entry));
                } else {
                    self.dispatch(entry);
                }
            }
        }
    }

    // ── History pre-forming (pure, no signals) ────────────────────

    /// Convert a sequence of `LogEntry` envelopes into a flat `Vec<UiMessage>`,
    /// plus the final session name, running state, and turn state.
    ///
    /// Pure function — no reactive side effects.  `AssistantText` chunks
    /// are concatenated into a single `AssistantFinal`; tool calls and
    /// results bracket them properly.  The returned `TurnState` reflects
    /// whether the last message is a `Thinking` that was never followed
    /// by a content event — this lets the caller initialise the live FSM
    /// correctly.
    ///
    /// Each entry's real `seq` (from the envelope) is used — not a counted
    /// value — so overlay/compaction targeting stays correct even on a forked
    /// branch whose seqs are non-contiguous.
    fn build_messages(entries: &[LogEntry], start_id: usize) -> BuildResult {
        let mut messages: Vec<UiMessage> = Vec::with_capacity(entries.len());
        let mut session_name: Option<String> = None;
        let mut running = false;
        let mut streaming: String = String::new();
        // Seq of the first `AssistantText` chunk in the pending stream.
        let mut streaming_seq: Option<u64> = None;
        let mut next_id = start_id;
        let mut turn = TurnState::Idle;
        let mut context_usage: Option<(usize, usize)> = None;

        for entry in entries {
            let event_seq = entry.seq;
            match &entry.event {
                SessionEvent::SessionInfo { name } => {
                    session_name = Some(name.clone());
                }
                SessionEvent::UserPrompt { content, .. } => {
                    running = true;
                    turn = TurnState::Active;
                    messages.push(UiMessage::UserPrompt {
                        id: next_id,
                        seq: event_seq,
                        content: content.clone(),
                        deleted: RwSignal::new(false),
                        edit: RwSignal::new(None),
                    });
                    next_id += 1;
                }
                SessionEvent::Thinking => {
                    running = true;
                    // Server always pairs Thinking with a follow-up event,
                    // but if this is the last event in the buffer we need
                    // to keep the placeholder so the live FSM can remove it.
                    turn = TurnState::Thinking;
                    messages.push(UiMessage::Thinking { id: next_id });
                    next_id += 1;
                }
                SessionEvent::AssistantText(chunk) => {
                    leave_thinking(&mut messages, &mut turn);
                    if streaming.is_empty() {
                        streaming_seq = Some(event_seq);
                    }
                    streaming.push_str(chunk);
                }
                SessionEvent::ToolCall {
                    id: tool_id,
                    name,
                    arguments,
                } => {
                    leave_thinking(&mut messages, &mut turn);
                    flush_streaming_to(
                        &mut messages,
                        &mut streaming,
                        &mut streaming_seq,
                        &mut next_id,
                    );
                    let args = flatten_args(arguments);
                    messages.push(UiMessage::ToolCall {
                        id: next_id,
                        seq: event_seq,
                        tool_id: tool_id.clone(),
                        name: name.clone(),
                        args,
                        result: RwSignal::new(None),
                        result_seq: RwSignal::new(None),
                        expanded: RwSignal::new(false),
                        deleted: RwSignal::new(false),
                        edit: RwSignal::new(None),
                        result_edit: RwSignal::new(None),
                    });
                    next_id += 1;
                }
                SessionEvent::ToolResult { id, content } => {
                    // ToolResult can arrive while still in Thinking state
                    // (server emits Thinking after ToolResult).  But in
                    // the history buffer ToolResult always follows
                    // ToolCall, which already left thinking.
                    flush_streaming_to(
                        &mut messages,
                        &mut streaming,
                        &mut streaming_seq,
                        &mut next_id,
                    );
                    // Attach result to the most-recent ToolCall without a
                    // result (same logic as the live dispatch path).
                    if let Some(UiMessage::ToolCall {
                        result: r,
                        result_seq: rs,
                        ..
                    }) = messages.last_mut()
                        && r.get_untracked().is_none()
                    {
                        r.set(Some(content.clone()));
                        rs.set(Some(event_seq));
                    }
                    // Mark the tool id as consumed (no separate UiMessage).
                    let _ = id;
                }
                SessionEvent::TurnEnded { reason } => {
                    running = false;
                    leave_thinking(&mut messages, &mut turn);
                    match reason {
                        TurnEndReason::Completed | TurnEndReason::StreamEnded => {
                            flush_streaming_to(
                                &mut messages,
                                &mut streaming,
                                &mut streaming_seq,
                                &mut next_id,
                            );
                            messages.push(UiMessage::FinalResponse { id: next_id });
                            next_id += 1;
                        }
                        TurnEndReason::Cancelled { .. } => {
                            streaming.clear();
                            streaming_seq = None;
                            messages.push(UiMessage::Cancelled { id: next_id });
                            next_id += 1;
                        }
                        reason @ (TurnEndReason::MaxTurnsExceeded { .. }
                        | TurnEndReason::Error { .. }) => {
                            flush_streaming_to(
                                &mut messages,
                                &mut streaming,
                                &mut streaming_seq,
                                &mut next_id,
                            );
                            let msg = reason.error_message().unwrap_or_default();
                            messages.push(UiMessage::Error { id: next_id, msg });
                            next_id += 1;
                        }
                    }
                    turn = TurnState::Idle;
                }
                SessionEvent::SessionState { .. } => {
                    // running is derived from message content above.
                }
                SessionEvent::ContextUsage { used, limit } => {
                    context_usage = Some((*used, *limit));
                }
                // ── compaction ──
                // A rolling LLM summary replaced some earlier messages.
                // Group them (and everything after, up to this point) into a
                // collapsible `CompactedGroup`.
                SessionEvent::Compacted {
                    summary,
                    model,
                    covers,
                    manual,
                } => {
                    apply_compaction(
                        &mut messages,
                        next_id,
                        event_seq,
                        summary,
                        model,
                        *manual,
                        covers,
                    );
                    next_id += 1;
                }
                // ── tool-pair summarization ──
                SessionEvent::ToolSummarized {
                    id: tool_id,
                    summary,
                    model,
                } => {
                    apply_tool_summary(&mut messages, next_id, event_seq, tool_id, summary, model);
                    next_id += 1;
                }
                // ── overlay events ──
                SessionEvent::Edited {
                    target,
                    replacement,
                } => {
                    apply_edit(&mut messages, *target, replacement);
                }
                SessionEvent::Deleted { target } => {
                    apply_delete(&mut messages, *target);
                }
                // ── metadata events (no UI) ──
                SessionEvent::ContextSnapshot { .. } | SessionEvent::ModelChanged { .. } => {}
                SessionEvent::HistoryComplete => {
                    // Never appears in an envelope — the subscriber yields it
                    // as a bare `ServerMessage::HistoryComplete`.
                }
            }
        }

        // Trailing streaming text (shouldn't regularly happen, but be safe).
        flush_streaming_to(
            &mut messages,
            &mut streaming,
            &mut streaming_seq,
            &mut next_id,
        );

        BuildResult {
            messages,
            session_name,
            running,
            turn_state: turn,
            next_id,
            context_usage,
        }
    }

    // ── Live event dispatch ───────────────────────────────────────

    /// Dispatch a single `SessionEvent` into UI state.
    ///
    /// Only called for live events (after `HistoryComplete`).
    /// An exhaustive match ensures every new event variant added to
    /// `goop-shared` produces a compile error here until handled.
    fn dispatch(&self, entry: LogEntry) {
        // The real transaction-log seq from the envelope — not a counted
        // value.  This stays correct on a forked branch whose seqs are
        // non-contiguous (the counter hack is gone).
        let seq = entry.seq;

        let next_id = || {
            let id = self.next_message_id.get_untracked();
            self.next_message_id.set(id + 1);
            id
        };

        // Helper: transition out of Thinking, removing the placeholder
        // message.  No-ops if already in Active or Idle.
        let leave_thinking_signal = |state: &Self| {
            if state.turn_state.get_untracked().has_thinking_msg() {
                state.turn_state.set(TurnState::Active);
                state.messages.update(|ms| {
                    // Defensive: only pop if the last message really is
                    // Thinking.  The FSM guarantees this, but the signal
                    // and the message list could in theory diverge from a
                    // bug elsewhere.
                    if matches!(ms.last(), Some(UiMessage::Thinking { .. })) {
                        ms.pop();
                    }
                });
            }
        };

        match entry.event {
            SessionEvent::SessionInfo { name } => {
                self.current_session.set(Some(name));
            }
            SessionEvent::UserPrompt { content, .. } => {
                self.btn_state.set(BtnState::Running);
                self.turn_state.set(TurnState::Active);
                let id = next_id();
                self.messages.update(|ms| {
                    ms.push(UiMessage::UserPrompt {
                        id,
                        seq,
                        content,
                        deleted: RwSignal::new(false),
                        edit: RwSignal::new(None),
                    });
                });
            }
            SessionEvent::Thinking => {
                // The server emits Thinking at the start of each turn
                // and after each ToolResult.  Always push a placeholder
                // and enter the Thinking state.
                self.turn_state.set(TurnState::Thinking);
                let id = next_id();
                self.messages
                    .update(|ms| ms.push(UiMessage::Thinking { id }));
            }
            SessionEvent::AssistantText(chunk) => {
                leave_thinking_signal(self);
                // Record the first chunk's seq for the eventual AssistantFinal.
                if self.streaming_text.get_untracked().is_empty() {
                    self.streaming_seq.set(Some(seq));
                }
                self.streaming_text.update(|s| s.push_str(&chunk));
            }
            SessionEvent::ToolCall {
                id: tool_id,
                name,
                arguments,
            } => {
                leave_thinking_signal(self);
                self.flush_streaming();
                let args = flatten_args(&arguments);
                let id = next_id();
                self.messages.update(|ms| {
                    ms.push(UiMessage::ToolCall {
                        id,
                        seq,
                        tool_id,
                        name,
                        args,
                        result: RwSignal::new(None),
                        result_seq: RwSignal::new(None),
                        expanded: RwSignal::new(false),
                        deleted: RwSignal::new(false),
                        edit: RwSignal::new(None),
                        result_edit: RwSignal::new(None),
                    });
                });
            }
            SessionEvent::ToolResult { id, content } => {
                // ToolResult doesn't change the turn state — the server
                // follows it with a Thinking event that will push a new
                // placeholder.
                self.flush_streaming();
                // Find the most-recent ToolCall whose result hasn't been
                // filled yet and attach this result.  Searching backwards
                // from the end is O(n) but the scan stops at the first
                // match, which is almost always the very last message.
                // This is robust against history replay (which doesn't
                // know about indices) and against multiple in-flight tool
                // calls (though the server emits them one at a time).
                self.messages.update(|ms| {
                    for msg in ms.iter_mut().rev() {
                        if let UiMessage::ToolCall {
                            result: r,
                            result_seq: rs,
                            ..
                        } = msg
                            && r.get_untracked().is_none()
                        {
                            r.set(Some(content));
                            rs.set(Some(seq));
                            return;
                        }
                    }
                });
                let _ = id;
            }
            SessionEvent::TurnEnded { reason } => {
                self.btn_state.update(|s| *s = BtnState::on_llm_done(*s));
                leave_thinking_signal(self);
                match reason {
                    TurnEndReason::Completed | TurnEndReason::StreamEnded => {
                        let raw = self.streaming_text.get_untracked();
                        let raw_seq = self.streaming_seq.get_untracked();
                        self.streaming_text.set(String::new());
                        self.streaming_seq.set(None);
                        if !raw.is_empty() {
                            let id = next_id();
                            self.messages.update(|ms| {
                                ms.push(UiMessage::AssistantFinal {
                                    id,
                                    seq: raw_seq.unwrap_or(0),
                                    raw,
                                    deleted: RwSignal::new(false),
                                    edit: RwSignal::new(None),
                                });
                            });
                        }
                        let id = next_id();
                        self.messages
                            .update(|ms| ms.push(UiMessage::FinalResponse { id }));
                    }
                    TurnEndReason::Cancelled { .. } => {
                        self.streaming_text.set(String::new());
                        self.streaming_seq.set(None);
                        let id = next_id();
                        self.messages
                            .update(|ms| ms.push(UiMessage::Cancelled { id }));
                    }
                    reason @ (TurnEndReason::MaxTurnsExceeded { .. }
                    | TurnEndReason::Error { .. }) => {
                        self.flush_streaming();
                        let msg = reason.error_message().unwrap_or_default();
                        let id = next_id();
                        self.messages
                            .update(|ms| ms.push(UiMessage::Error { id, msg }));
                    }
                }
                self.turn_state.set(TurnState::Idle);
            }
            SessionEvent::SessionState { .. } => {
                // running is derived from message content.
            }
            SessionEvent::ContextUsage { used, limit } => {
                self.context_usage.set(Some((used, limit)));
            }
            // ── compaction ──
            // A rolling LLM summary replaced some earlier messages.  Group
            // them (and everything after, up to now) into a collapsible
            // `CompactedGroup`.
            SessionEvent::Compacted {
                summary,
                model,
                covers,
                manual,
            } => {
                let id = next_id();
                self.messages.update(|ms| {
                    apply_compaction(ms, id, seq, &summary, &model, manual, &covers);
                });
            }
            // ── tool-pair summarization ──
            SessionEvent::ToolSummarized {
                id: tool_id,
                summary,
                model,
            } => {
                let id = next_id();
                self.messages.update(|ms| {
                    apply_tool_summary(ms, id, seq, &tool_id, &summary, &model);
                });
            }
            // ── overlay events ──
            SessionEvent::Edited {
                target,
                replacement,
            } => {
                self.messages.update(|ms| {
                    apply_edit(ms, target, &replacement);
                });
            }
            SessionEvent::Deleted { target } => {
                self.messages.update(|ms| {
                    apply_delete(ms, target);
                });
            }
            // ── metadata events (no UI) ──
            SessionEvent::ContextSnapshot { .. } | SessionEvent::ModelChanged { .. } => {}
            SessionEvent::HistoryComplete => {
                // Handled in handle_subscribed() — a no-op here.
            }
        }
    }

    /// If there's pending streaming text, finalize it as an `AssistantFinal`
    /// message.  Called before tool calls, tool results, errors, etc.
    fn flush_streaming(&self) {
        let text = self.streaming_text.get_untracked();
        if !text.is_empty() {
            let seq = self.streaming_seq.get_untracked().unwrap_or(0);
            self.streaming_text.set(String::new());
            self.streaming_seq.set(None);
            let id = self.next_message_id.get_untracked();
            self.next_message_id.set(id + 1);
            self.messages.update(|ms| {
                ms.push(UiMessage::AssistantFinal {
                    id,
                    seq,
                    raw: text,
                    deleted: RwSignal::new(false),
                    edit: RwSignal::new(None),
                });
            });
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────

/// Transition out of the `Thinking` state: set turn to `Active` and pop the
/// trailing `UiMessage::Thinking`.  No-ops if already in `Active` or
/// `Idle`.
///
/// Used by the pure `build_messages` path (takes `&mut` references instead
/// of signals).
fn leave_thinking(messages: &mut Vec<UiMessage>, turn: &mut TurnState) {
    if turn.has_thinking_msg() {
        *turn = TurnState::Active;
        if matches!(messages.last(), Some(UiMessage::Thinking { .. })) {
            messages.pop();
        }
    }
}

/// Flush accumulated streaming text into the message vec as an
/// `AssistantFinal`.  Clears `streaming` and `streaming_seq`.  Increments
/// `next_id`.  The `AssistantFinal`'s seq is the *first* chunk's seq
/// (matching how the server merges consecutive `AssistantText` events into
/// one agent-visible item).
fn flush_streaming_to(
    messages: &mut Vec<UiMessage>,
    streaming: &mut String,
    streaming_seq: &mut Option<u64>,
    next_id: &mut usize,
) {
    if !streaming.is_empty() {
        messages.push(UiMessage::AssistantFinal {
            id: *next_id,
            seq: streaming_seq.unwrap_or(0),
            raw: std::mem::take(streaming),
            deleted: RwSignal::new(false),
            edit: RwSignal::new(None),
        });
        *streaming_seq = None;
        *next_id += 1;
    }
}

/// Flatten `serde_json::Value::Object` into `Vec<(String, String)>`.
fn flatten_args(arguments: &serde_json::Value) -> Vec<(String, String)> {
    match arguments {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), val)
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ── compaction & overlay helpers ────────────────────────────────────
//
// These operate on `&mut [UiMessage]` (or `&mut Vec<UiMessage>`) so they
// can be shared by the pure `build_messages` path and the signal-backed
// `dispatch` path (which calls them inside `self.messages.update(..)`).

/// Whether a message's originating event seq is in `covers`.  A `ToolCall`
/// matches on either its call seq or its result seq (both are agent-visible
/// and thus both appear in `Compacted.covers`).
fn message_in_covers(msg: &UiMessage, covers: &HashSet<u64>) -> bool {
    match msg {
        UiMessage::ToolCall {
            seq, result_seq, ..
        } => {
            covers.contains(seq)
                || result_seq
                    .get_untracked()
                    .is_some_and(|rs| covers.contains(&rs))
        }
        m => m.agent_seq().is_some_and(|s| covers.contains(&s)),
    }
}

/// Group messages covered by a `Compacted` event into a collapsible
/// [`UiMessage::CompactedGroup`].
///
/// **Auto-compaction** (`manual = false`): `covers` spans the entire
/// agent-visible prefix, so we group from the first covered message to the
/// end of the list — including trailing UI-only markers (Thinking,
/// FinalResponse) that belong to the compacted turns.  Existing groups
/// (from earlier compactions or tool summaries) become children, giving
/// nested/recursive trees naturally.
///
/// **Manual compaction** (`manual = true`): `covers` is a contiguous range.
/// We group from the first to the last covered message — including any
/// non-agent-visible messages (Thinking, etc.) between them, which belong
/// to the compacted turns.  Messages before and after the range stay in
/// place.
fn apply_compaction(
    messages: &mut Vec<UiMessage>,
    id: usize,
    seq: u64,
    summary: &str,
    model: &str,
    manual: bool,
    covers: &[u64],
) {
    let cover_set: HashSet<u64> = covers.iter().copied().collect();
    let Some(start) = messages
        .iter()
        .position(|m| message_in_covers(m, &cover_set))
    else {
        // Nothing covered — defence in depth (shouldn't happen: the server
        // only emits Compacted when there are ≥ 2 agent-visible items).
        return;
    };

    let end = if manual {
        // Contiguous range: from first to last covered message.
        messages
            .iter()
            .rposition(|m| message_in_covers(m, &cover_set))
            .unwrap_or(start)
    } else {
        // Auto: everything from the first covered message to the end.
        messages.len() - 1
    };

    let children: Vec<UiMessage> = messages.drain(start..=end).collect();
    messages.insert(
        start,
        UiMessage::CompactedGroup {
            id,
            seq,
            summary: summary.to_string(),
            model: model.to_string(),
            manual,
            children,
            expanded: RwSignal::new(false),
        },
    );
}

/// Wrap the `ToolCall` whose logical id matches `tool_id` in a
/// [`UiMessage::ToolSummaryGroup`].  Searches top-level messages and
/// recurses into `CompactedGroup` children (a pair can survive inside a
/// not-yet-compacted group).  Returns `true` if found and wrapped.
fn apply_tool_summary(
    messages: &mut [UiMessage],
    id: usize,
    seq: u64,
    tool_id: &str,
    summary: &str,
    model: &str,
) -> bool {
    for msg in messages.iter_mut() {
        if matches!(msg, UiMessage::ToolCall { tool_id: t, .. } if t.as_str() == tool_id) {
            // Take the ToolCall out and put the group in its place.  The
            // sentinel is immediately overwritten, so its value is irrelevant.
            let child = std::mem::replace(msg, UiMessage::Thinking { id: 0 });
            *msg = UiMessage::ToolSummaryGroup {
                id,
                seq,
                summary: summary.to_string(),
                model: model.to_string(),
                child: Box::new(child),
                expanded: RwSignal::new(false),
            };
            return true;
        }
        if let UiMessage::CompactedGroup { children, .. } = msg
            && apply_tool_summary(children, id, seq, tool_id, summary, model)
        {
            return true;
        }
    }
    false
}

/// Whether `target` matches this message's originating seq (call or result
/// seq for a `ToolCall`).
fn message_matches_target(msg: &UiMessage, target: u64) -> bool {
    match msg {
        UiMessage::UserPrompt { seq, .. } | UiMessage::AssistantFinal { seq, .. } => *seq == target,
        UiMessage::ToolCall {
            seq, result_seq, ..
        } => *seq == target || result_seq.get_untracked() == Some(target),
        _ => false,
    }
}

/// Apply an `Edited` overlay to the message whose seq is `target`.
/// Recurses into group children.  Returns `true` if found.
fn apply_edit(messages: &mut [UiMessage], target: u64, replacement: &EditContent) -> bool {
    for msg in messages.iter_mut() {
        if message_matches_target(msg, target) {
            apply_edit_to_message(msg, replacement);
            return true;
        }
        let found = match msg {
            UiMessage::CompactedGroup { children, .. } => apply_edit(children, target, replacement),
            UiMessage::ToolSummaryGroup { child, .. } => {
                apply_edit(std::slice::from_mut(child.as_mut()), target, replacement)
            }
            _ => false,
        };
        if found {
            return true;
        }
    }
    false
}

/// Apply an `Edited` overlay to a single message, based on the replacement
/// content type.  The original content stays in the message's fields; the
/// overlay holds the replacement so the UI can toggle between them.
fn apply_edit_to_message(msg: &mut UiMessage, replacement: &EditContent) {
    match (msg, replacement) {
        (UiMessage::UserPrompt { edit, .. }, EditContent::Text(text)) => {
            edit.set(Some(EditOverlay::new(text.clone())));
        }
        (UiMessage::AssistantFinal { edit, .. }, EditContent::Text(text)) => {
            edit.set(Some(EditOverlay::new(text.clone())));
        }
        (UiMessage::ToolCall { edit, .. }, EditContent::ToolCall { name, arguments }) => {
            let args = flatten_args(arguments);
            edit.set(Some(EditOverlay::new(format_tool_call_display(
                name, &args,
            ))));
        }
        (UiMessage::ToolCall { result_edit, .. }, EditContent::ToolResult { content }) => {
            result_edit.set(Some(EditOverlay::new(content.clone())));
        }
        // Mismatched replacement type for the target variant — no-op
        // (defence in depth; the server targets the right type).
        _ => {}
    }
}

/// Mark the message whose seq is `target` as deleted.  Recurses into group
/// children.  Returns `true` if found.
fn apply_delete(messages: &mut [UiMessage], target: u64) -> bool {
    for msg in messages.iter_mut() {
        if message_matches_target(msg, target) {
            set_deleted(msg);
            return true;
        }
        let found = match msg {
            UiMessage::CompactedGroup { children, .. } => apply_delete(children, target),
            UiMessage::ToolSummaryGroup { child, .. } => {
                apply_delete(std::slice::from_mut(child.as_mut()), target)
            }
            _ => false,
        };
        if found {
            return true;
        }
    }
    false
}

/// Set the `deleted` flag on an editable message.  No-op for variants
/// without a `deleted` flag (groups — editing/deleting a summary is a
/// future concern; see Phase 8).
fn set_deleted(msg: &mut UiMessage) {
    match msg {
        UiMessage::UserPrompt { deleted, .. }
        | UiMessage::AssistantFinal { deleted, .. }
        | UiMessage::ToolCall { deleted, .. } => deleted.set(true),
        _ => {}
    }
}

/// Format a tool call (name + args) as a display string for an edit overlay.
fn format_tool_call_display(name: &str, args: &[(String, String)]) -> String {
    let args_str = args
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    if args_str.is_empty() {
        name.to_string()
    } else {
        format!("{name}\n{args_str}")
    }
}

impl EditOverlay {
    fn new(replacement: String) -> Self {
        Self {
            replacement,
            show_original: RwSignal::new(false),
        }
    }
}
