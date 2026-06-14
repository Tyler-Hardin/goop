use std::sync::Arc;

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::client::{CompletionClient, ProviderClient};
use rig::memory::InMemoryConversationMemory;
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::events::SessionEvent;
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
    agent: Arc<rig::agent::Agent<deepseek::CompletionModel>>,
    tx: broadcast::Sender<SessionEvent>,
    history: Mutex<Vec<SessionEvent>>,

    /// Push a prompt here from any view; the background worker drains it.
    submit_tx: mpsc::UnboundedSender<String>,
}

impl Session {
    /// Create a new session backed by a DeepSeek agent.
    ///
    /// `capacity` controls how many live events the broadcast channel
    /// can buffer between the slowest and fastest subscriber.
    ///
    /// A background task is spawned to drain the prompt queue — the
    /// tokio runtime must already be running.
    pub fn new(capacity: usize) -> anyhow::Result<Arc<Self>> {
        let client = deepseek::Client::from_env()?;
        let agent = client
            .agent(deepseek::DEEPSEEK_V4_PRO)
            .tool(tools::Read)
            .tool(tools::Replace)
            .tool(tools::Shell)
            .tool(tools::Write)
            .max_tokens(100_000)
            .default_max_turns(100)
            .memory(InMemoryConversationMemory::new())
            .conversation_id("default")
            .build();

        let (tx, _) = broadcast::channel(capacity);
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();

        let this = Arc::new(Self {
            agent: Arc::new(agent),
            tx,
            history: Mutex::new(Vec::new()),
            submit_tx,
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
    pub fn submit(&self, prompt: impl Into<String>) {
        // Unbounded send never fails.
        let _ = self.submit_tx.send(prompt.into());
    }

    // ── subscribe ────────────────────────────────────────────────

    /// Subscribe to **live events only**.
    ///
    /// Use this for views that have been present since session creation
    /// and don't need a history replay.
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
    async fn drain_queue(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<String>) {
        while let Some(prompt) = rx.recv().await {
            self.emit(SessionEvent::UserPrompt {
                content: prompt.clone(),
            })
            .await;
            self.run_one(&prompt).await;
        }
    }

    /// Process a single prompt through the agent, emitting events.
    async fn run_one(&self, prompt: &str) {
        self.emit(SessionEvent::Thinking).await;

        let mut stream = self.agent.stream_prompt(prompt).await;

        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    text,
                ))) => {
                    self.emit(SessionEvent::AssistantText(text.text)).await;
                }

                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ToolCall { tool_call, .. },
                )) => {
                    let args = match serde_json::from_str::<serde_json::Value>(
                        &tool_call.function.arguments.to_string(),
                    ) {
                        Ok(v) => v,
                        Err(_) => {
                            serde_json::Value::String(tool_call.function.arguments.to_string())
                        }
                    };
                    self.emit(SessionEvent::ToolCall {
                        name: tool_call.function.name,
                        arguments: args,
                    })
                    .await;
                }

                Ok(MultiTurnStreamItem::StreamUserItem(
                    rig::streaming::StreamedUserContent::ToolResult { tool_result, .. },
                )) => {
                    let text: String = tool_result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            rig::message::ToolResultContent::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    self.emit(SessionEvent::ToolResult { content: text }).await;
                    self.emit(SessionEvent::Thinking).await;
                }

                Ok(MultiTurnStreamItem::FinalResponse(_response)) => {
                    self.emit(SessionEvent::FinalResponse).await;
                    return;
                }

                Ok(_) => {}

                Err(e) => {
                    self.emit(SessionEvent::Error(e.to_string())).await;
                    return;
                }
            }
        }

        // If the stream ended without FinalResponse, still signal completion.
        self.emit(SessionEvent::FinalResponse).await;
    }

    /// Send an event to live subscribers and append to history.
    async fn emit(&self, event: SessionEvent) {
        let _ = self.tx.send(event.clone());
        self.history.lock().await.push(event);
    }
}
