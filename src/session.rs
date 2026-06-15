use std::path::PathBuf;
use std::sync::Arc;

use chrono::Local;
use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::client::{CompletionClient, ProviderClient};
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::events::{PromptSource, SessionEvent};
use crate::memory::FileConversationMemory;
use crate::memory::prompt_history_path;
use crate::preamble::build_preamble;
use crate::tools;

// ── subscriber with history replay ──────────────────────────────

/// Returned by [`Session::subscribe_all`]. Replays every prior event
/// before yielding live events.
#[allow(dead_code)] // used by future views (web, phone, …)
pub struct SessionSubscriber {
    history: Vec<SessionEvent>,
    rx: broadcast::Receiver<SessionEvent>,
}

#[allow(dead_code)] // used by future views
impl SessionSubscriber {
    /// Wait for the next event (history first, then live).
    pub async fn recv(&mut self) -> Result<SessionEvent, broadcast::error::RecvError> {
        if !self.history.is_empty() {
            return Ok(self.history.remove(0));
        }
        self.rx.recv().await
    }
}

// ── session ─────────────────────────────────────────────────────

/// Holds the agent, conversation state, and a serialised prompt queue.
///
/// Multiple views can submit prompts and subscribe to events
/// concurrently — the session guarantees prompts are processed
/// one at a time in FIFO order.
pub struct Session {
    /// Session name (user-supplied or auto-generated like `20260128_001`).
    name: String,
    agent: Arc<rig::agent::Agent<deepseek::CompletionModel>>,
    tx: broadcast::Sender<SessionEvent>,
    history: Mutex<Vec<SessionEvent>>,

    /// Push a prompt here from any view; the background worker drains it.
    /// Each entry carries an optional completion signal for the submitter.
    submit_tx: mpsc::UnboundedSender<(String, PromptSource, Option<oneshot::Sender<()>>)>,

    /// Set by `cancel()` and consumed by the currently-running turn.
    /// When the sender is dropped or fired, the turn is cancelled.
    cancel_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,

    /// If set, every event is appended to this file as JSONL.
    events_file: Option<PathBuf>,
}

impl Session {
    /// Create a new session backed by a DeepSeek agent.
    ///
    /// `capacity` controls how many live events the broadcast channel
    /// can buffer between the slowest and fastest subscriber.
    ///
    /// Session data is persisted to
    /// `~/.config/goop/sessions/<name>.jsonl` (events) and
    /// `~/.config/goop/sessions/<name>.messages.jsonl` (agent memory).
    /// Existing files are loaded so named sessions can be resumed.
    ///
    /// A background task is spawned to drain the prompt queue — the
    /// tokio runtime must already be running.
    pub fn new(capacity: usize, session_name: Option<String>) -> anyhow::Result<Arc<Self>> {
        let client = deepseek::Client::from_env()?;

        // ── persistence paths ──────────────────────────────────
        let name = session_name.unwrap_or_else(next_session_name);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let events_path = dir.join(format!("{name}.jsonl"));
        let messages_path = dir.join(format!("{name}.messages.jsonl"));
        let mem = FileConversationMemory::new(messages_path)?;
        // Load pre-existing events for history replay.
        let existing_events = load_events_from_file(&events_path).unwrap_or_default();

        let preamble = build_preamble();

        let agent = client
            .agent(deepseek::DEEPSEEK_V4_PRO)
            .preamble(&preamble)
            .tool(tools::Read)
            .tool(tools::ReadHtml)
            .tool(tools::Replace)
            .tool(tools::Shell)
            .tool(tools::WebFetch)
            .tool(tools::Write)
            .tool(tools::Screenshot)
            .tool(tools::CursorPosition)
            .tool(tools::MouseMove)
            .tool(tools::MouseClick)
            .tool(tools::KeyType)
            .tool(tools::KeyPress)
            .tool(tools::WindowList)
            .tool(tools::WindowFocus)
            .tool(tools::WindowGetActive)
            .tool(tools::OpenUrl)
            .max_tokens(100_000)
            .default_max_turns(100)
            .conversation_id("default")
            .memory(mem)
            .build();

        let (tx, _) = broadcast::channel(capacity);
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();

        let session_info = SessionEvent::SessionInfo { name: name.clone() };
        // Ensure SessionInfo is first in history for replay to new clients.
        let mut existing_events = existing_events;
        if existing_events.is_empty()
            || !matches!(
                existing_events.first(),
                Some(SessionEvent::SessionInfo { .. })
            )
        {
            existing_events.insert(0, session_info.clone());
        }
        // Broadcast immediately so live subscribers see it.
        let _ = tx.send(session_info);

        let this = Arc::new(Self {
            name: name.clone(),
            agent: Arc::new(agent),
            tx,
            history: Mutex::new(existing_events),
            submit_tx,
            cancel_tx: std::sync::Mutex::new(None),
            events_file: Some(events_path),
        });

        // Spawn the background worker that serializes prompt processing.
        let worker = Arc::clone(&this);
        tokio::spawn(async move {
            worker.drain_queue(submit_rx).await;
        });

        Ok(this)
    }

    /// Submit a prompt from any view.  Returns immediately; the prompt
    /// is queued and processed when earlier submissions finish.
    ///
    /// Returns a receiver that fires when this prompt completes
    /// (i.e. FinalResponse or Error has been emitted).
    pub fn submit(&self, prompt: impl Into<String>, source: PromptSource) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        // Unbounded send never fails.
        let _ = self.submit_tx.send((prompt.into(), source, Some(tx)));
        rx
    }

    /// Cancel the currently-running LLM turn (if any).
    /// Safe to call from any thread / async context; idempotent.
    pub fn cancel(&self) {
        if let Some(tx) = self.cancel_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// Return the session name (user-supplied or auto-generated).
    pub fn name(&self) -> &str {
        &self.name
    }

    // ── subscribe ────────────────────────────────────────────────

    /// Subscribe to **live events only**.
    ///
    /// Use this for views that have been present since session creation
    /// and don't need a history replay.
    #[allow(dead_code)] // useful for future views that don't need history replay
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.tx.subscribe()
    }

    /// Subscribe with **full history replay**.
    ///
    /// Late-joining views (web, phone, …) receive every event since
    /// session creation before transitioning to live events.
    #[allow(dead_code)] // used by future views
    pub async fn subscribe_all(&self) -> SessionSubscriber {
        let history = self.history.lock().await.clone();
        let rx = self.tx.subscribe();
        SessionSubscriber { history, rx }
    }

    // ── internals ────────────────────────────────────────────────

    /// Background worker: drain prompts one at a time.
    async fn drain_queue(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<(String, PromptSource, Option<oneshot::Sender<()>>)>,
    ) {
        while let Some((prompt, source, done)) = rx.recv().await {
            // Write every prompt to the global history file (all sources).
            append_prompt_to_history(&prompt).await;

            self.emit(SessionEvent::UserPrompt {
                content: prompt.clone(),
                source,
            })
            .await;
            self.run_one(&prompt).await;
            // Notify the submitter that this prompt is done.
            if let Some(tx) = done {
                let _ = tx.send(());
            }
        }
    }

    /// Process a single prompt through the agent, emitting events.
    async fn run_one(&self, prompt: &str) {
        // Set up cancellation for this turn.
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        *self.cancel_tx.lock().unwrap() = Some(cancel_tx);

        self.emit(SessionEvent::Thinking).await;

        let mut stream = self.agent.stream_prompt(prompt).await;

        loop {
            tokio::select! {
                // Bias: check cancel first so a queued cancel wins
                // even if a stream item happens to be ready.
                biased;

                _ = &mut cancel_rx => {
                    self.emit(SessionEvent::Cancelled).await;
                    return;
                }

                item = stream.next() => {
                    match item {
                        Some(Ok(MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::Text(text),
                        ))) => {
                            self.emit(SessionEvent::AssistantText(text.text)).await;
                        }

                        Some(Ok(MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ToolCall { tool_call, .. },
                        ))) => {
                            let args = match serde_json::from_str::<serde_json::Value>(
                                &tool_call.function.arguments.to_string(),
                            ) {
                                Ok(v) => v,
                                Err(_) => {
                                    serde_json::Value::String(
                                        tool_call.function.arguments.to_string(),
                                    )
                                }
                            };
                            self.emit(SessionEvent::ToolCall {
                                name: tool_call.function.name,
                                arguments: args,
                            })
                            .await;
                        }

                        Some(Ok(MultiTurnStreamItem::StreamUserItem(
                            rig::streaming::StreamedUserContent::ToolResult {
                                tool_result, ..
                            },
                        ))) => {
                            let text: String = tool_result
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    rig::message::ToolResultContent::Text(t) => {
                                        Some(t.text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("");

                            self.emit(SessionEvent::ToolResult { content: text }).await;
                            self.emit(SessionEvent::Thinking).await;
                        }

                        Some(Ok(MultiTurnStreamItem::FinalResponse(_response))) => {
                            self.clear_cancel();
                            self.emit(SessionEvent::FinalResponse).await;
                            return;
                        }

                        Some(Ok(_)) => {}

                        Some(Err(e)) => {
                            self.clear_cancel();
                            self.emit(SessionEvent::Error(e.to_string())).await;
                            return;
                        }

                        None => {
                            // Stream ended without FinalResponse.
                            self.clear_cancel();
                            self.emit(SessionEvent::FinalResponse).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Remove the cancel sender for the current turn (turn ending normally).
    fn clear_cancel(&self) {
        self.cancel_tx.lock().unwrap().take();
    }

    /// Send an event to live subscribers, append to history, and
    /// persist to the events file (if one is configured).
    async fn emit(&self, event: SessionEvent) {
        // Persist to file before broadcasting so the file is always
        // ahead of any subscriber that might race a crash.
        if let Some(ref path) = self.events_file {
            append_event_to_file(path, &event).await;
        }
        let _ = self.tx.send(event.clone());
        self.history.lock().await.push(event);
    }
}

// ── persistence helpers ─────────────────────────────────────────

/// Directory for session files: `~/.config/goop/sessions/`
fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("."));
    PathBuf::from(home)
        .join(".config")
        .join("goop")
        .join("sessions")
}

/// Auto-generate a session name like `20260128_001`.
///
/// Scans `~/.config/goop/sessions/` for files matching today's
/// `YYYYMMDD_` prefix, finds the highest sequence number, and
/// returns the next one.
pub(crate) fn next_session_name() -> String {
    let today = Local::now().format("%Y%m%d").to_string();
    let dir = sessions_dir();
    let prefix = format!("{today}_");

    let mut max_seq = 0u32;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(&prefix) {
                // Take the sequence number before any suffix (e.g. "001.jsonl" → "001")
                let num_str = rest.split('.').next().unwrap_or(rest);
                if let Ok(num) = num_str.parse::<u32>()
                    && num > max_seq
                {
                    max_seq = num;
                }
            }
        }
    }

    format!("{today}_{:03}", max_seq + 1)
}

/// Load session events from a JSONL file.
fn load_events_from_file(path: &std::path::Path) -> Result<Vec<SessionEvent>, anyhow::Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)?;
    let mut events = Vec::new();
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: SessionEvent = serde_json::from_str(&line)?;
        events.push(event);
    }
    Ok(events)
}

/// Append a single event as a JSON line to the events file.
async fn append_event_to_file(path: &std::path::Path, event: &SessionEvent) {
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("failed to serialize event: {e}");
            return;
        }
    };
    // Use tokio async file I/O so we don't block the runtime.
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("failed to open events file {path:?}: {e}");
            return;
        }
    };
    if let Err(e) = file.write_all(format!("{json}\n").as_bytes()).await {
        tracing::error!("failed to write event to {path:?}: {e}");
    }
}

/// Append a prompt to the global prompt history file as a JSON-encoded
/// string (handles multi-line prompts safely).
async fn append_prompt_to_history(prompt: &str) {
    let path = prompt_history_path();
    // Ensure the parent directory exists.
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let json = serde_json::to_string(prompt).unwrap_or_else(|_| {
        // Fallback: just serialize the empty string so we don't lose the
        // fact that a prompt was submitted.
        String::from("\"\"")
    });
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("failed to open history file {path:?}: {e}");
            return;
        }
    };
    if let Err(e) = file.write_all(format!("{json}\n").as_bytes()).await {
        tracing::error!("failed to write to history file {path:?}: {e}");
    }
}
