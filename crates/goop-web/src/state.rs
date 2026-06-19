use goop_shared::{ClientMessage, SessionEvent, TurnEndReason};
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

/// UI-facing message type.  Derived from `SessionEvent` by the dispatch
/// function — keeps raw event shapes out of the component tree.
///
/// Each message carries a unique `id` so `<For>` (keyed iteration) can
/// track individual messages across re-renders.  Without stable keys,
/// every `messages` signal update recreates all DOM nodes, retriggering
/// CSS animations (flash).
#[derive(Clone, Debug)]
pub enum UiMessage {
    UserPrompt {
        id: usize,
        content: String,
    },
    Thinking {
        id: usize,
    },
    AssistantFinal {
        id: usize,
        raw: String,
    },
    ToolCall {
        id: usize,
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
        expanded: RwSignal<bool>,
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
    /// Raw `SessionEvent`s accumulated during history replay.  No signals
    /// are touched while `connection` is `CatchingUp` — every event lands
    /// here.  On `HistoryComplete` the buffer is pre-formed into a single
    /// `Vec<UiMessage>` and all signals are set in one shot.
    pub(crate) history_buffer: RwSignal<Vec<SessionEvent>>,

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
}

/// Result of pre-forming buffered history events into UI state.
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
            turn_state: RwSignal::new(TurnState::Idle),
            sidebar_open: RwSignal::new(false),
            input_text: RwSignal::new(String::new()),
            next_message_id: RwSignal::new(0),
            context_usage: RwSignal::new(None),
            history_buffer: RwSignal::new(Vec::new()),
            connection_gen: RwSignal::new(0),
            input_focus_request: RwSignal::new(0),
            reconnect_attempt: RwSignal::new(0),
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

    /// Handle an incoming `SessionEvent` from the WebSocket.
    ///
    /// During history catch-up (`connection == CatchingUp`), events are
    /// buffered without touching any reactive signals.  When
    /// `HistoryComplete` arrives the entire buffer is pre-formed into
    /// a single `Vec<UiMessage>` and all signals are set in one shot —
    /// the page materialises instantly, as if the chat had already
    /// happened.
    ///
    /// In live mode, each event is dispatched immediately.
    pub fn handle_event(&self, event: SessionEvent) {
        if self.connection.get_untracked().is_catching_up() {
            match event {
                SessionEvent::HistoryComplete => {
                    let buffer = self.history_buffer.get_untracked();
                    self.history_buffer.set(Vec::new());
                    self.connection.set(ConnectionState::Connected);

                    // Pre-form all messages from the buffered events
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
                other => {
                    self.history_buffer.update(|buf| buf.push(other));
                }
            }
        } else {
            self.dispatch(event);
        }
    }

    // ── History pre-forming (pure, no signals) ────────────────────

    /// Convert a sequence of `SessionEvent`s into a flat `Vec<UiMessage>`,
    /// plus the final session name, running state, and turn state.
    ///
    /// Pure function — no reactive side effects.  `AssistantText` chunks
    /// are concatenated into a single `AssistantFinal`; tool calls and
    /// results bracket them properly.  The returned `TurnState` reflects
    /// whether the last message is a `Thinking` that was never followed
    /// by a content event — this lets the caller initialise the live FSM
    /// correctly.
    fn build_messages(events: &[SessionEvent], start_id: usize) -> BuildResult {
        let mut messages: Vec<UiMessage> = Vec::with_capacity(events.len());
        let mut session_name: Option<String> = None;
        let mut running = false;
        let mut streaming: String = String::new();
        let mut next_id = start_id;
        let mut turn = TurnState::Idle;
        let mut context_usage: Option<(usize, usize)> = None;

        for event in events {
            match event {
                SessionEvent::SessionInfo { name } => {
                    session_name = Some(name.clone());
                }
                SessionEvent::UserPrompt { content, .. } => {
                    running = true;
                    turn = TurnState::Active;
                    messages.push(UiMessage::UserPrompt {
                        id: next_id,
                        content: content.clone(),
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
                    streaming.push_str(chunk);
                }
                SessionEvent::ToolCall {
                    name, arguments, ..
                } => {
                    leave_thinking(&mut messages, &mut turn);
                    flush_streaming_to(&mut messages, &mut streaming, &mut next_id);
                    let args = flatten_args(arguments);
                    let expanded = RwSignal::new(false);
                    let result = RwSignal::new(None);
                    messages.push(UiMessage::ToolCall {
                        id: next_id,
                        name: name.clone(),
                        args,
                        result,
                        expanded,
                    });
                    next_id += 1;
                }
                SessionEvent::ToolResult { content, .. } => {
                    // ToolResult can arrive while still in Thinking state
                    // (server emits Thinking after ToolResult).  But in
                    // the history buffer ToolResult always follows
                    // ToolCall, which already left thinking.
                    flush_streaming_to(&mut messages, &mut streaming, &mut next_id);
                    // Attach result to the most-recent ToolCall without a
                    // result (same logic as the live dispatch path).
                    if let Some(UiMessage::ToolCall { result: r, .. }) = messages.last_mut()
                        && r.get_untracked().is_none()
                    {
                        r.set(Some(content.clone()));
                    }
                }
                SessionEvent::TurnEnded { reason } => {
                    running = false;
                    leave_thinking(&mut messages, &mut turn);
                    match reason {
                        TurnEndReason::Completed | TurnEndReason::StreamEnded => {
                            flush_streaming_to(&mut messages, &mut streaming, &mut next_id);
                            messages.push(UiMessage::FinalResponse { id: next_id });
                            next_id += 1;
                        }
                        TurnEndReason::Cancelled { .. } => {
                            streaming.clear();
                            messages.push(UiMessage::Cancelled { id: next_id });
                            next_id += 1;
                        }
                        reason @ (TurnEndReason::MaxTurnsExceeded { .. }
                        | TurnEndReason::Error { .. }) => {
                            flush_streaming_to(&mut messages, &mut streaming, &mut next_id);
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
                // ── compaction / overlay / metadata events ──
                // Not yet emitted by the server (later phases).  No UI yet.
                SessionEvent::Compacted { .. }
                | SessionEvent::ToolSummarized { .. }
                | SessionEvent::ContextSnapshot { .. }
                | SessionEvent::ModelChanged { .. }
                | SessionEvent::Edited { .. }
                | SessionEvent::Deleted { .. } => {}
                SessionEvent::HistoryComplete => {
                    // Should never appear in the buffer — the caller
                    // intercepts it before buffering.
                }
            }
        }

        // Trailing streaming text (shouldn't normally happen, but be safe).
        flush_streaming_to(&mut messages, &mut streaming, &mut next_id);

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
    fn dispatch(&self, event: SessionEvent) {
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

        match event {
            SessionEvent::SessionInfo { name } => {
                self.current_session.set(Some(name));
            }
            SessionEvent::UserPrompt { content, .. } => {
                self.btn_state.set(BtnState::Running);
                self.turn_state.set(TurnState::Active);
                let id = next_id();
                self.messages
                    .update(|ms| ms.push(UiMessage::UserPrompt { id, content }));
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
                self.streaming_text.update(|s| s.push_str(&chunk));
            }
            SessionEvent::ToolCall {
                name, arguments, ..
            } => {
                leave_thinking_signal(self);
                self.flush_streaming();
                let args = flatten_args(&arguments);
                let expanded = RwSignal::new(false);
                let result = RwSignal::new(None);
                let id = next_id();
                self.messages.update(|ms| {
                    ms.push(UiMessage::ToolCall {
                        id,
                        name,
                        args,
                        result,
                        expanded,
                    });
                });
            }
            SessionEvent::ToolResult { content, .. } => {
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
                        if let UiMessage::ToolCall { result: r, .. } = msg
                            && r.get_untracked().is_none()
                        {
                            r.set(Some(content));
                            return;
                        }
                    }
                });
            }
            SessionEvent::TurnEnded { reason } => {
                self.btn_state.update(|s| *s = BtnState::on_llm_done(*s));
                leave_thinking_signal(self);
                match reason {
                    TurnEndReason::Completed | TurnEndReason::StreamEnded => {
                        let raw = self.streaming_text.get_untracked();
                        self.streaming_text.set(String::new());
                        if !raw.is_empty() {
                            let id = next_id();
                            self.messages
                                .update(|ms| ms.push(UiMessage::AssistantFinal { id, raw }));
                        }
                        let id = next_id();
                        self.messages
                            .update(|ms| ms.push(UiMessage::FinalResponse { id }));
                    }
                    TurnEndReason::Cancelled { .. } => {
                        self.streaming_text.set(String::new());
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
            // ── compaction / overlay / metadata events ──
            // Not yet emitted by the server (later phases).  No UI yet.
            SessionEvent::Compacted { .. }
            | SessionEvent::ToolSummarized { .. }
            | SessionEvent::ContextSnapshot { .. }
            | SessionEvent::ModelChanged { .. }
            | SessionEvent::Edited { .. }
            | SessionEvent::Deleted { .. } => {}
            SessionEvent::HistoryComplete => {
                // Handled in handle_event() — a no-op here.
            }
        }
    }

    /// If there's pending streaming text, finalize it as an `AssistantFinal`
    /// message.  Called before tool calls, tool results, errors, etc.
    fn flush_streaming(&self) {
        let text = self.streaming_text.get_untracked();
        if !text.is_empty() {
            self.streaming_text.set(String::new());
            let id = self.next_message_id.get_untracked();
            self.next_message_id.set(id + 1);
            self.messages
                .update(|ms| ms.push(UiMessage::AssistantFinal { id, raw: text }));
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
/// `AssistantFinal`.  Clears `streaming`.  Increments `next_id`.
fn flush_streaming_to(messages: &mut Vec<UiMessage>, streaming: &mut String, next_id: &mut usize) {
    if !streaming.is_empty() {
        messages.push(UiMessage::AssistantFinal {
            id: *next_id,
            raw: std::mem::take(streaming),
        });
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
