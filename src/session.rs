use std::sync::Arc;

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::client::{CompletionClient, ProviderClient};
use rig::memory::InMemoryConversationMemory;
use rig::providers::deepseek;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::events::{PromptSource, SessionEvent};
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
    /// Each entry carries an optional completion signal for the submitter.
    submit_tx: mpsc::UnboundedSender<(String, PromptSource, Option<oneshot::Sender<()>>)>,

    /// Set by `cancel()` and consumed by the currently-running turn.
    /// When the sender is dropped or fired, the turn is cancelled.
    cancel_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
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

        // ── Preamble (via template) ──────────────────────────────
        //
        // Order is deliberate to maximise prompt-cache prefix re-use:
        //   1. Static guidelines (never changes)
        //   2. User + OS info (changes only with system upgrades)
        //   3. USER.md (persistent user memory; changes rarely)
        //   4. CWD (changes per session / cd)
        //   5. AGENTS.md (project context; may be edited mid-session)

        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        let home = std::env::var("HOME").unwrap_or_else(|_| String::from("~"));
        let shell = std::env::var("SHELL").unwrap_or_else(|_| String::from("/bin/sh"));
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| String::from("(unknown)"));

        // USER.md
        let user_md = if let Ok(home_dir) = std::env::var("HOME") {
            let user_md_path = std::path::PathBuf::from(&home_dir)
                .join(".config")
                .join("goop")
                .join("USER.md");
            if !user_md_path.exists() {
                let _ = std::fs::create_dir_all(user_md_path.parent().unwrap());
                let _ = std::fs::write(&user_md_path, "");
            }
            let content = std::fs::read_to_string(&user_md_path).unwrap_or_default();
            let trimmed = content.trim();
            if trimmed.is_empty() {
                String::from("[empty, no user memories yet.]")
            } else {
                trimmed.to_string()
            }
        } else {
            String::from("[empty, no user memories yet.]")
        };

        // AGENTS.md (may be present or absent — template handles the conditional)
        let agents_md = std::fs::read_to_string("AGENTS.md").ok();

        let mut context = tera::Context::new();
        context.insert("user", &user);
        context.insert("home", &home);
        context.insert("shell", &shell);
        context.insert("os_family", std::env::consts::OS);
        context.insert("arch", std::env::consts::ARCH);
        context.insert("os_distro", &os_release());
        context.insert("cwd", &cwd);
        context.insert("user_md", &user_md);
        if let Some(ref amd) = agents_md {
            context.insert("agents_md", amd);
        }

        let template = include_str!("preamble.md");
        let preamble = tera::Tera::one_off(template, &context, false)
            .expect("failed to render preamble template");

        let agent = client
            .agent(deepseek::DEEPSEEK_V4_PRO)
            .preamble(&preamble)
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
            cancel_tx: std::sync::Mutex::new(None),
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
    async fn drain_queue(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<(String, PromptSource, Option<oneshot::Sender<()>>)>,
    ) {
        while let Some((prompt, source, done)) = rx.recv().await {
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

    /// Send an event to live subscribers and append to history.
    async fn emit(&self, event: SessionEvent) {
        let _ = self.tx.send(event.clone());
        self.history.lock().await.push(event);
    }
}

// ── OS detection helper ─────────────────────────────────────────

/// Returns a human-readable OS/distro string, e.g. "NixOS 24.11" or
/// "macOS 15.2".  Falls back to `uname -sr` output.
fn os_release() -> String {
    // Linux: try /etc/os-release first
    if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
        let mut name = None;
        let mut version = None;
        for line in contents.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                // PRETTY_NAME="NixOS 24.11 (Vicuna)"  →  "NixOS 24.11"
                let v = v.trim_matches('"');
                // Strip trailing parenthetical like " (Vicuna)"
                if let Some(paren) = v.rfind(" (") {
                    return v[..paren].to_string();
                }
                return v.to_string();
            }
            if let Some(v) = line.strip_prefix("NAME=") {
                name = Some(v.trim_matches('"').to_string());
            }
            if let Some(v) = line.strip_prefix("VERSION=") {
                version = Some(v.trim_matches('"').to_string());
            }
        }
        if let Some(n) = name {
            if let Some(v) = version {
                return format!("{n} {v}");
            }
            return n;
        }
    }

    // macOS / BSD / fallback: use uname
    if let Ok(output) = std::process::Command::new("uname")
        .args(["-s", "-r"])
        .output()
        && output.status.success()
    {
        return String::from_utf8_lossy(&output.stdout).trim().to_string();
    }

    // Last resort
    std::env::consts::OS.to_string()
}
