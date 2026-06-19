use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Local;
use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::Message;
use rig::message;
use rig::streaming::StreamedAssistantContent;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::config::{self, Config, McpServerDef};
use crate::events::{LogEntry, PromptSource, SessionEvent, TurnEndReason};
use crate::memory::{
    self, LogReplayMemory, TransactionLog, build_session_memory, prompt_history_path,
};
use crate::model;
use crate::preamble::build_preamble;
use crate::session_state::{PersistedSessionState, SessionState};
use crate::transport::{PersistedTransport, Transport};

// ── subscriber with history replay ──────────────────────────────

/// Returned by [`Session::subscribe_all`]. Replays every prior event
/// before yielding live events.
pub struct SessionSubscriber {
    history: Vec<LogEntry>,
    history_cursor: usize,
    history_done: bool,
    rx: broadcast::Receiver<SessionEvent>,
}

impl SessionSubscriber {
    /// Wait for the next event (history first, then live).
    ///
    /// After the last history event, the next call returns
    /// [`SessionEvent::HistoryComplete`] to signal the transition to
    /// live events.
    pub async fn recv(&mut self) -> Result<SessionEvent, broadcast::error::RecvError> {
        if self.history_cursor < self.history.len() {
            let event = self.history[self.history_cursor].event.clone();
            self.history_cursor += 1;
            return Ok(event);
        }
        if !self.history_done {
            self.history_done = true;
            return Ok(SessionEvent::HistoryComplete);
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
    /// Shared mutable state (CWD, transport, home_dir) accessible by tools.
    ///
    /// Held here to keep the `Arc` alive; tools receive their own clone
    /// at construction time via [`model::build_agent`].
    #[allow(dead_code)]
    pub(crate) state: Arc<SessionState>,
    agent: Arc<crate::model::AnyAgent>,
    tx: broadcast::Sender<SessionEvent>,
    history: Arc<Mutex<TransactionLog>>,

    /// Clone of the log-replay memory, used to estimate token counts for
    /// the context-usage progress bar.  Shares the same transaction-log
    /// `Arc<Mutex<TransactionLog>>` as the agent's memory, so it always
    /// reflects the latest conversation state.
    memory: LogReplayMemory,

    /// Context window limit (in tokens) for the progress bar.  Uses the
    /// compaction budget when compaction is enabled, otherwise the model's
    /// known context window.  `None` when neither is known.
    context_limit: Option<usize>,

    /// Token budget at which the agent-visible conversation is compacted into a
    /// rolling LLM summary (see [`maybe_compact`](Self::maybe_compact)).
    /// `None` disables compaction (unlimited context).
    compaction_threshold: Option<usize>,

    /// The session's model string (e.g. `deepseek/deepseek-v4-pro`), recorded
    /// in `Compacted` events for "which model produced this summary".
    model_label: String,

    /// Push a prompt here from any view; the background worker drains it.
    /// Each entry carries an optional completion signal for the submitter.
    submit_tx: mpsc::UnboundedSender<(String, PromptSource, Option<oneshot::Sender<()>>)>,

    /// Set by `cancel()` and consumed by the currently-running turn.
    /// When the sender is dropped or fired, the turn is cancelled.
    ///
    /// Uses a tokio [`Mutex`] so cancel (called from async WS handler) and
    /// clear_cancel (called from async run_one) never hold a blocking lock.
    cancel_tx: Mutex<Option<oneshot::Sender<()>>>,

    /// Whether the session is currently processing a prompt.
    /// Used to tell late-joining clients whether to show a Cancel button.
    is_running: AtomicBool,

    /// Per-session MCP manager — kept alive so peer connections stay open
    /// for tool invocation. Shared MCP servers live in [`SessionManager`].
    #[allow(dead_code)]
    session_mcp: Arc<crate::mcp::McpManager>,

    /// Push notification sender — called when a prompt completes so PWAs
    /// in the background get a system notification.
    push_notifier: Arc<crate::push::PushManager>,

    /// Speech-to-text engine — shared across all sessions, loaded lazily.
    /// `None` if STT is not configured (disabled in config).
    stt: Option<Arc<crate::stt::SpeechToText>>,
}

impl Session {
    /// Create a new session backed by the configured provider.
    ///
    /// `capacity` controls how many live events the broadcast channel
    /// can buffer between the slowest and fastest subscriber.
    ///
    /// Session data is persisted to
    /// `~/.config/goop/sessions/<name>.jsonl` (the append-only transaction
    /// log — the single source of truth for both UI history and agent
    /// memory) and `~/.config/goop/sessions/<name>.state.toml`
    /// (config + CWD + transport).  Existing files are loaded so named
    /// sessions can be resumed.
    ///
    /// If the session was previously SSH'd, the SSH connection is
    /// re-established synchronously (awaited) before the session is
    /// returned — no race between reconnect and first prompt.
    ///
    /// A background task is spawned to drain the prompt queue — the
    /// tokio runtime must already be running.
    pub async fn new(
        config: &Config,
        capacity: usize,
        session_name: Option<String>,
        shared_mcp_manager: Arc<crate::mcp::McpManager>,
        push_notifier: Arc<crate::push::PushManager>,
        stt: Option<Arc<crate::stt::SpeechToText>>,
    ) -> anyhow::Result<Arc<Self>> {
        // ── persistence paths ──────────────────────────────────
        let name = session_name.unwrap_or_else(next_session_name);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let events_path = dir.join(format!("{name}.jsonl"));
        let state_path = crate::session_state::state_path(&name);

        // ── load persisted state (config overrides + CWD + transport) ──
        let persisted = PersistedSessionState::load(&name).unwrap_or_default();

        // Merge session config overrides into the global config.
        let merged_config = persisted.config.merge(config);

        let initial_local_cwd = persisted.local_cwd.clone();

        // ── SessionState (created before agent so tools can use it) ──
        let state = Arc::new(SessionState::new(
            config.home_dir.clone(),
            initial_local_cwd.clone(),
            persisted.config.clone(),
            state_path,
        ));

        // ── restore SSH transport synchronously ────────────────
        let initial_cwd_for_preamble = match &persisted.transport {
            PersistedTransport::Local => {
                // Nothing to restore — already local.
                initial_local_cwd
            }
            PersistedTransport::Ssh {
                destination,
                remote_cwd,
            } => {
                match crate::ssh::ssh_connect(destination, None).await {
                    Ok(transport) => {
                        // Resolve remote CWD.
                        let resolved_cwd = if let Transport::Ssh(ref ssh_state) = transport {
                            match transport.canonicalize(remote_cwd).await {
                                Ok(canon) => {
                                    *ssh_state.remote_cwd.lock().await = canon.clone();
                                    canon
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "SSH {name}: could not canonicalize persisted CWD \
                                         {remote_cwd:?}: {e}"
                                    );
                                    remote_cwd.clone()
                                }
                            }
                        } else {
                            remote_cwd.clone()
                        };

                        state.set_transport(transport).await;
                        state.save().await;
                        tracing::info!("SSH {name} → {destination} reconnected");
                        resolved_cwd
                    }
                    Err(e) => {
                        tracing::warn!("SSH {name} → {destination} reconnect failed: {e}");
                        // Fall back to local — the session is usable locally.
                        initial_local_cwd
                    }
                }
            }
        };

        let preamble = build_preamble(
            &initial_cwd_for_preamble.display().to_string(),
            &config.home_dir,
        );

        // ── MCP servers ────────────────────────────────────────
        // Resolve which servers to enable: global ∪ session overrides.
        let session_names = crate::mcp::resolve(config, persisted.config.mcp_server_names());

        // Build the list of (name, def) pairs for per-session servers.
        let session_servers: Vec<(String, McpServerDef)> = session_names
            .iter()
            .filter_map(|n| config.mcp_servers.get(n).map(|d| (n.clone(), d.clone())))
            .collect();

        // Connect per-session MCP servers (always creates a manager, even
        // if the list is empty — empty manager is a no-op sentinel).
        let session_mcp_manager = crate::mcp::McpManager::connect(&session_servers).await;

        // Collect MCP tool proxies from shared and session managers.
        let mut mcp_tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        mcp_tools.extend(shared_mcp_manager.build_tools());
        mcp_tools.extend(session_mcp_manager.build_tools());

        let (tx, _) = broadcast::channel(capacity);
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();

        // ── open the transaction log (RAII: loads, migrates, injects
        //    SessionInfo, persists if new) ──────────────────────────
        let log = TransactionLog::open(events_path, &name).await?;
        // Broadcast SessionInfo immediately so live subscribers see it.
        // (The log already has it; this is the delivery mechanism.)
        let _ = tx.send(SessionEvent::SessionInfo { name: name.clone() });

        // ── shared transaction log: the source of truth for agent memory ──
        // The same `Arc` is handed to the `LogReplayMemory` below, so every
        // event emitted via `emit()` is visible to `ConversationMemory::load`.
        let history = Arc::new(Mutex::new(log));

        let mem = build_session_memory(history.clone());
        // Clone for the context-usage progress bar (shares the log `Arc`).
        let memory_for_usage = mem.clone();

        // Context window limit for the progress bar — the model's known
        // window.  `None` hides the bar when the window is unknown.
        let context_limit =
            memory::lookup_context_length(merged_config.provider(), merged_config.model_name())
                .map(|v| v as usize);

        // Token budget at which the agent-visible conversation is compacted
        // into a rolling LLM summary.  An absolute token count, or a
        // percentage of the model's context window.
        let compaction_threshold = resolve_compaction_threshold(&merged_config);

        let model_label = merged_config.model.to_string();

        let agent = model::build_agent(&merged_config, &preamble, mem, state.clone(), mcp_tools)?;

        let this = Arc::new(Self {
            name: name.clone(),
            state: state.clone(),
            agent,
            tx,
            history,
            memory: memory_for_usage,
            context_limit,
            compaction_threshold,
            model_label,
            submit_tx,
            cancel_tx: Mutex::new(None),
            is_running: AtomicBool::new(false),
            session_mcp: session_mcp_manager,
            push_notifier,
            stt,
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
    /// (i.e. a [`TurnEnded`](SessionEvent::TurnEnded) event has been
    /// emitted).
    pub fn submit(&self, prompt: impl Into<String>, source: PromptSource) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        // Unbounded send never fails.
        let _ = self.submit_tx.send((prompt.into(), source, Some(tx)));
        rx
    }

    /// Cancel the currently-running LLM turn (if any).
    /// Safe to call from any async context; idempotent.
    pub async fn cancel(&self) {
        if let Some(tx) = self.cancel_tx.lock().await.take() {
            let _ = tx.send(());
        }
    }

    /// Submit audio for speech-to-text transcription.
    ///
    /// The WAV-encoded audio is transcribed using the server's local
    /// Whisper model.  On success the resulting text is submitted as a
    /// normal prompt via [`submit`](Self::submit).  On failure a
    /// [`TurnEnded`](SessionEvent::TurnEnded) event with an
    /// [`Error`](TurnEndReason::Error) reason is emitted.
    ///
    /// Returns immediately if STT is not configured.
    pub async fn submit_audio(&self, wav_bytes: Vec<u8>, source: PromptSource) {
        let stt = match self.stt.as_ref() {
            Some(s) => s,
            None => {
                self.emit(SessionEvent::TurnEnded {
                    reason: TurnEndReason::Error {
                        message: "STT is not enabled — set [stt] enabled = true in config.toml"
                            .into(),
                    },
                })
                .await;
                return;
            }
        };

        match stt.transcribe_wav(&wav_bytes).await {
            Ok(text) => {
                tracing::info!("STT → {:?}", text);
                self.submit(text, source);
            }
            Err(e) => {
                tracing::warn!("STT failed: {e}");
                self.emit(SessionEvent::TurnEnded {
                    reason: TurnEndReason::Error {
                        message: format!("STT: {e}"),
                    },
                })
                .await;
            }
        }

        // Debug: write the last audio to /tmp/goop-last-audio.wav so the
        // user can listen to what whisper received.  Overwritten each time.
        if let Err(e) = tokio::fs::write("/tmp/goop-last-audio.wav", &wav_bytes).await {
            tracing::warn!("failed to write debug WAV: {e}");
        }
    }

    /// Return the session name (user-supplied or auto-generated).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the session is currently processing a prompt.
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
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
    pub async fn subscribe_all(&self) -> SessionSubscriber {
        let history = self.history.lock().await.entries_vec();
        let rx = self.tx.subscribe();
        SessionSubscriber {
            history,
            history_cursor: 0,
            history_done: false,
            rx,
        }
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

            // Compact the agent-visible conversation if it has grown past the
            // configured budget, so the next turn stays within context.
            self.maybe_compact().await;

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

            // If the restart tool was called during this prompt, the
            // flag is now set.  Signal the server to shut down gracefully
            // and stop processing further prompts.
            if crate::server::is_restart_requested() {
                crate::server::notify_shutdown();
                break;
            }
        }
    }

    /// If the agent-visible conversation has grown past the compaction
    /// threshold, summarize it into a rolling `Compacted` event so the next
    /// turn stays within the context budget.  The entire agent-visible prefix
    /// is covered; the in-progress prompt (handled by rig) is preserved, as is
    /// goose's "keep the most-recent user message" behaviour.  Summaries are
    /// themselves agent-visible, so later compactions summarize the prior
    /// summary — a rolling summary.
    ///
    /// No-op when compaction is disabled (`threshold == None`) or the
    /// conversation is still small.
    async fn maybe_compact(&self) {
        let Some(threshold) = self.compaction_threshold else {
            return;
        };
        let items = self.memory.agent_visible_items().await;
        if items.len() < 2 {
            return;
        }
        let messages: Vec<Message> = items.iter().map(|i| i.msg.clone()).collect();
        let tokens = self.memory.count_tokens(&messages);
        if tokens < threshold {
            return;
        }

        // Cover every agent-visible item; the current (in-progress) prompt is
        // not among them and is preserved by rig appending it itself.
        let covers: Vec<u64> = items.iter().map(|i| i.seq).collect();
        tracing::info!(
            "compacting {} agent-visible items (~{} tokens >= threshold {threshold})",
            items.len(),
            tokens
        );
        match self
            .agent
            .summarize(messages, COMPACTION_SYSTEM_PROMPT)
            .await
        {
            Ok(summary) => {
                self.emit(SessionEvent::Compacted {
                    summary,
                    model: self.model_label.clone(),
                    covers,
                    manual: false,
                })
                .await;
            }
            Err(e) => {
                // Keep the full history this turn; it'll be retried next prompt.
                tracing::warn!("compaction summarization failed; keeping full history: {e:#}");
            }
        }
    }

    /// Process a single prompt through the agent, emitting events.
    async fn run_one(&self, prompt: &str) {
        // Set up cancellation for this turn.
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        *self.cancel_tx.lock().await = Some(cancel_tx);

        self.is_running.store(true, Ordering::SeqCst);
        self.emit(SessionEvent::Thinking).await;

        // Audit trail: record the agent-visible context (post-compaction,
        // post-overlay) the LLM is about to see, plus the model.  One per
        // user→agent transition.  The in-progress prompt is appended by rig
        // itself, so the snapshot captures the committed memory context.
        {
            let items = self.memory.agent_visible_items().await;
            self.emit(SessionEvent::ContextSnapshot {
                seqs: items.iter().map(|i| i.seq).collect(),
                model: self.model_label.clone(),
            })
            .await;
        }

        // Whether any tool call has *completed* (ToolCall + matching
        // ToolResult) this turn.  This drives the cancel-recovery
        // decision: a cancel with committed work is recorded as
        // `TurnEnded::Cancelled { prompt: None }` (the turn stays
        // agent-visible on replay); a cancel with no work is recorded
        // with `prompt: Some(_)` (the whole turn is dropped on replay,
        // and the prompt is handed back to the terminal for editing).
        // No explicit memory-preservation is needed — the events log is
        // the source of truth and `TurnEnded`'s reason controls
        // visibility during replay.
        let mut committed_work = false;

        {
            let mut stream = self.agent.stream_prompt(prompt).await;

            loop {
                tokio::select! {
                    // Bias: check cancel first so a queued cancel wins
                    // even if a stream item happens to be ready.
                    biased;

                    _ = &mut cancel_rx => {
                        // ── cancellation ──────────────────────────────────
                        // The events for whatever streamed so far are already
                        // in the log; the `TurnEnded` reason is all replay
                        // needs to decide visibility.  An in-flight tool
                        // call (emitted, no result) is dropped by the
                        // replay's orphan safety net.
                        if !committed_work {
                            // Nothing completed — return the prompt for editing.
                            self.is_running.store(false, Ordering::SeqCst);
                            self.emit(SessionEvent::TurnEnded {
                                reason: TurnEndReason::Cancelled {
                                    prompt: Some(prompt.to_string()),
                                },
                            })
                            .await;
                            self.notify_push("Cancelled").await;
                            return;
                        }

                        self.is_running.store(false, Ordering::SeqCst);
                        self.emit(SessionEvent::TurnEnded {
                            reason: TurnEndReason::Cancelled { prompt: None },
                        })
                        .await;
                        self.emit_context_usage().await;
                        self.notify_push("Cancelled").await;
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
                                let id = tool_call.id.clone();
                                let name = tool_call.function.name.clone();
                                self.emit(SessionEvent::ToolCall {
                                    id,
                                    name,
                                    arguments: args,
                                })
                                .await;
                            }

                            Some(Ok(MultiTurnStreamItem::StreamUserItem(
                                rig::streaming::StreamedUserContent::ToolResult {
                                    tool_result,
                                    ..
                                },
                            ))) => {
                                let id = tool_result.id.clone();
                                let text: String = tool_result
                                    .content
                                    .iter()
                                    .filter_map(|c| match c {
                                        message::ToolResultContent::Text(t) => {
                                            Some(t.text.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("");

                                // A result arrived for a tool call this turn —
                                // the turn has committed work, so a later cancel
                                // keeps it agent-visible on replay.
                                committed_work = true;

                                self.emit(SessionEvent::ToolResult { id, content: text })
                                    .await;
                                self.emit(SessionEvent::Thinking).await;
                            }

                            Some(Ok(MultiTurnStreamItem::FinalResponse(_response))) => {
                                self.clear_cancel().await;
                                self.emit(SessionEvent::TurnEnded {
                                    reason: TurnEndReason::Completed,
                                })
                                .await;
                                self.emit_context_usage().await;
                                self.notify_push("Completed").await;
                                return;
                            }

                            Some(Ok(_)) => {}

                            Some(Err(e)) => {
                                // ── error ──────────────────────────────────
                                // The events for whatever streamed so far are
                                // already in the log; committed work stays
                                // agent-visible on replay via the `TurnEnded`
                                // reason.  No explicit memory save is needed.
                                self.clear_cancel().await;

                                // Give the max-turns limit a structured
                                // reason (actionable message derived by the
                                // views); surface other errors verbatim.
                                let reason =
                                    if let rig::agent::StreamingError::Prompt(b) = &e
                                        && let rig::completion::PromptError::MaxTurnsError {
                                            max_turns,
                                            ..
                                        } = b.as_ref()
                                    {
                                    TurnEndReason::MaxTurnsExceeded {
                                        max_turns: *max_turns,
                                    }
                                    } else {
                                        TurnEndReason::Error {
                                            message: e.to_string(),
                                        }
                                    };
                                let label = reason.push_label();
                                self.emit(SessionEvent::TurnEnded { reason }).await;
                                self.emit_context_usage().await;
                                self.notify_push(label).await;
                                return;
                            }

                            None => {
                                // Stream ended without FinalResponse.
                                // Previously misrecorded as a clean
                                // FinalResponse — StreamEnded distinguishes
                                // the two.
                                self.clear_cancel().await;
                                self.emit(SessionEvent::TurnEnded {
                                    reason: TurnEndReason::StreamEnded,
                                })
                                .await;
                                self.emit_context_usage().await;
                                self.notify_push("Completed").await;
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Remove the cancel sender for the current turn (turn ending normally).
    async fn clear_cancel(&self) {
        self.cancel_tx.lock().await.take();
        self.is_running.store(false, Ordering::SeqCst);
    }

    /// Fire a push notification so PWAs in the background know the
    /// prompt has completed.  Spawned as a separate task so push
    /// delivery latency doesn't block the session.
    async fn notify_push(&self, event: &str) {
        let notifier = Arc::clone(&self.push_notifier);
        let name = self.name.clone();
        let event = event.to_string();
        tokio::spawn(async move {
            notifier.notify(&name, &event).await;
        });
    }

    /// Estimate the current context window usage and emit a
    /// [`SessionEvent::ContextUsage`] so connected clients can update their
    /// progress bar.  Called after each turn completes (a
    /// [`TurnEnded`](SessionEvent::TurnEnded) event other than a
    /// cancel-with-no-work).
    ///
    /// No-op when `context_limit` is `None` (unknown model + no compaction).
    async fn emit_context_usage(&self) {
        let Some(limit) = self.context_limit else {
            return;
        };
        let used = self.memory.estimated_tokens().await;
        self.emit(SessionEvent::ContextUsage { used, limit }).await;
    }

    /// Send an event to live subscribers, append to history, and
    /// persist to the on-disk log.
    ///
    /// `TransactionLog::append` assigns the next monotonic `seq`, computes
    /// `parent` from the last entry, and stamps `ts` — all under the history
    /// lock, so seq order, parent-pointer order, and file-append order can
    /// never diverge.  `persist` writes the JSONL line (best-effort).  Live
    /// subscribers receive the bare event over the WebSocket; the envelope
    /// lives in the on-disk log and the in-memory history (the sources for
    /// replay).
    ///
    /// The file write and broadcast stay under the lock deliberately:
    /// `subscribe_all` does `lock → snapshot entries → subscribe to tx`,
    /// so keeping `append + persist + send` atomic with respect to that
    /// ensures every subscriber sees each event exactly once (history XOR
    /// live — never both, never neither).
    async fn emit(&self, event: SessionEvent) {
        let mut log = self.history.lock().await;
        let entry = log.append(event.clone());
        // Persist to file before broadcasting so the file is always
        // ahead of any subscriber that might race a crash.
        log.persist(&entry).await;
        let _ = self.tx.send(event);
    }
}

// ── session manager ───────────────────────────────────────────────

/// Owns all active sessions, allowing concurrent creation, lookup,
/// listing, and deletion. The server holds a single [`SessionManager`]
/// and routes WebSocket connections to the right session by name.
pub struct SessionManager {
    sessions: tokio::sync::RwLock<std::collections::HashMap<String, Arc<Session>>>,
    config: Config,
    /// Shared MCP manager — holds connections to all `shared = true`
    /// servers enabled globally.  Always present (empty manager when
    /// no shared servers are configured).  Cloned into each new session.
    global_mcp: tokio::sync::RwLock<Arc<crate::mcp::McpManager>>,
    /// Push notification manager — cloned into each new session so
    /// prompts can trigger push notifications on completion.
    push_manager: Arc<crate::push::PushManager>,
    /// Speech-to-text engine — shared across all sessions, loaded lazily
    /// via [`init_stt`](Self::init_stt).  `None` until initialised or
    /// when STT is disabled in config.
    stt: tokio::sync::RwLock<Option<Arc<crate::stt::SpeechToText>>>,
}

impl SessionManager {
    pub fn new(config: Config, push_manager: Arc<crate::push::PushManager>) -> Self {
        Self {
            sessions: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            config,
            global_mcp: tokio::sync::RwLock::new(crate::mcp::McpManager::empty()),
            push_manager,
            stt: tokio::sync::RwLock::new(None),
        }
    }

    /// Connect to all globally-enabled shared MCP servers.
    ///
    /// Must be called after construction and before any session uses them.
    /// Replaces the empty sentinel manager even if no servers are configured.
    pub async fn init_global_mcp(&self) {
        let servers: Vec<(String, McpServerDef)> = self
            .config
            .enabled_mcp_servers
            .iter()
            .filter_map(|name| {
                let def = self.config.mcp_servers.get(name)?;
                if !def.shared {
                    return None;
                }
                Some((name.clone(), def.clone()))
            })
            .collect();

        let manager = crate::mcp::McpManager::connect(&servers).await;
        tracing::info!(
            "MCP shared — {} server(s), {} tool(s) total",
            servers.len(),
            manager.build_tools().len(),
        );
        *self.global_mcp.write().await = manager;
    }

    /// Load the speech-to-text engine (Whisper model).
    ///
    /// Must be called after construction and before any session uses STT.
    /// No-ops if STT is disabled in config.  Downloads the model on first
    /// use (one-time, cached to `~/.config/goop/models/whisper/`).
    pub async fn init_stt(&self) {
        if !self.config.stt.enabled {
            tracing::info!("STT is disabled in config — skipping model load");
            return;
        }

        let model = self.config.stt.model;
        let models_dir = crate::config::config_dir().join("models").join("whisper");

        match crate::stt::ensure_model(model, &models_dir).await {
            Ok(model_path) => match crate::stt::SpeechToText::load(&model_path).await {
                Ok(engine) => {
                    *self.stt.write().await = Some(Arc::new(engine));
                }
                Err(e) => {
                    tracing::error!("failed to load STT model: {e}");
                }
            },
            Err(e) => {
                tracing::error!("failed to ensure STT model: {e}");
            }
        }
    }

    /// Get an existing session or create one.
    ///
    /// If the session already exists in the map, returns it directly.
    /// Otherwise creates a new [`Session`] — which loads events,
    /// messages, and state (config overrides + CWD + transport) from
    /// disk if files exist for this name.  Session config overrides
    /// are merged into the global config before building the agent.
    ///
    /// If the session was previously SSH'd, the SSH connection is
    /// re-established before this returns (no race with first prompt).
    pub async fn get_or_create(&self, name: String) -> anyhow::Result<Arc<Session>> {
        // Fast path: read lock.
        {
            let sessions = self.sessions.read().await;
            if let Some(s) = sessions.get(&name) {
                return Ok(Arc::clone(s));
            }
        }
        // Slow path: create the session (may await SSH reconnect),
        // then insert under write lock.
        let shared_mcp = Arc::clone(&*self.global_mcp.read().await);
        let push_manager = Arc::clone(&self.push_manager);
        let stt = self.stt.read().await.clone();
        let session = Session::new(
            &self.config,
            256,
            Some(name.clone()),
            shared_mcp,
            push_manager,
            stt,
        )
        .await?;
        let mut sessions = self.sessions.write().await;
        // Double-check: another caller may have created it while we
        // were building the session.
        if let Some(s) = sessions.get(&name) {
            return Ok(Arc::clone(s));
        }
        sessions.insert(name.clone(), Arc::clone(&session));

        // If this session was previously closed, un-close it now.
        remove_closed_session(&name);

        Ok(session)
    }

    /// Create a new session with an auto-generated name like `20260128_001`.
    pub async fn create(&self, name: Option<String>) -> anyhow::Result<Arc<Session>> {
        let name = name.unwrap_or_else(next_session_name);
        self.get_or_create(name).await
    }

    /// List all currently loaded session names, sorted.
    pub async fn list(&self) -> Vec<String> {
        let sessions = self.sessions.read().await;
        let mut names: Vec<String> = sessions.keys().cloned().collect();
        names.sort();
        names
    }

    /// Remove a session from memory and mark it as closed.
    ///
    /// The session's disk files are *not* deleted.  The session is
    /// added to the closed list so it won't reappear in the sidebar on
    /// restart.  To bring it back, create a new session with the exact
    /// same name.
    ///
    /// Always writes to the closed list — even if the session isn't
    /// currently in memory.  This ensures stale sidebar clicks and
    /// sessions that were pruned from the map still get persisted.
    pub async fn delete(&self, name: &str) -> bool {
        let removed = self.sessions.write().await.remove(name).is_some();
        add_closed_session(name);
        removed
    }

    /// Scan the sessions directory and load all discovered sessions
    /// that haven't been explicitly closed by the user.
    ///
    /// Call once at server startup so the web UI immediately shows
    /// every session that has persisted data.
    pub async fn discover(&self) -> anyhow::Result<()> {
        let dir = sessions_dir();
        if !dir.exists() {
            return Ok(());
        }
        let closed = load_closed_sessions();
        let entries = std::fs::read_dir(&dir)?;
        let mut names = std::collections::HashSet::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            // Session event files look like "<name>.jsonl".
            // Legacy files ("<name>.messages.jsonl", "<name>.cwd") are
            // ignored — the events log is the single source of truth now.
            if let Some(stripped) = fname.strip_suffix(".jsonl")
                && !stripped.ends_with(".messages")
            {
                names.insert(stripped.to_string());
            }
            // Also discover from .state.toml files (in case there's no .jsonl yet).
            if let Some(stripped) = fname.strip_suffix(".state.toml") {
                names.insert(stripped.to_string());
            }
        }
        for name in names {
            // Skip sessions the user has explicitly closed.
            if closed.contains(&name) {
                continue;
            }
            // Ignore errors for individual sessions — a corrupt file
            // shouldn't prevent the server from starting.
            let _ = self.get_or_create(name).await;
        }
        Ok(())
    }
}

// ── persistence helpers ─────────────────────────────────────────

/// Directory for session files: `~/.config/goop/sessions/`
pub(crate) fn sessions_dir() -> PathBuf {
    config::config_dir().join("sessions")
}

/// Path to a session's state file (public for `config.rs`).
pub fn session_state_path(name: &str) -> PathBuf {
    crate::session_state::state_path(name)
}

/// Path to the closed-sessions list: `~/.config/goop/closed_sessions.json`
fn closed_sessions_path() -> PathBuf {
    config::config_dir().join("closed_sessions.json")
}

/// Load the set of session names the user has explicitly closed.
fn load_closed_sessions() -> std::collections::HashSet<String> {
    let path = closed_sessions_path();
    if !path.exists() {
        return std::collections::HashSet::new();
    }
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return std::collections::HashSet::new(),
    };
    serde_json::from_str::<Vec<String>>(&contents)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// Persist a session name to the closed list.
fn add_closed_session(name: &str) {
    let mut closed = load_closed_sessions();
    closed.insert(name.to_string());
    save_closed_sessions(&closed);
}

/// Remove a session name from the closed list (un-close).
fn remove_closed_session(name: &str) {
    let mut closed = load_closed_sessions();
    if closed.remove(name) {
        save_closed_sessions(&closed);
    }
}

fn save_closed_sessions(closed: &std::collections::HashSet<String>) {
    let path = closed_sessions_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut names: Vec<&String> = closed.iter().collect();
    names.sort();
    if let Ok(json) = serde_json::to_string_pretty(&names) {
        let _ = std::fs::write(&path, json);
    }
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

/// System prompt for compaction summarization (embedded at compile time).
///
/// Instructs the model to preserve the technical substance of the
/// conversation — decisions, code, file paths, errors, pending work —
/// while dropping narration and redundant tool output, so the summary can
/// stand in for the earlier conversation.
const COMPACTION_SYSTEM_PROMPT: &str = "\
You are summarizing an earlier portion of a conversation between a user and \
an AI coding assistant (goop). The conversation has grown too long and must be \
compressed so work can continue within a limited context window. Produce a \
concise but complete summary that preserves everything needed to keep working.

Preserve in detail:
- The user's goals, requests, requirements, and any stated constraints or \
preferences.
- Key decisions made and the reasoning behind them.
- Technical specifics: file paths, code snippets, shell commands, APIs, \
library/framework names and versions, and configuration.
- Errors, stack traces, and how they were resolved (or remain unresolved).
- The current state of every task: what is done, what is in progress, what \
is pending, and the concrete next step.
- Any open questions, TODOs, or follow-ups.

Drop:
- Pleasantries, narration of the assistant's thought process, and redundant \
back-and-forth.
- Verbatim tool output that is no longer needed — but keep the facts it \
established (file listings, counts, the relevant excerpts).

Format the result as prose with short sections or bullet points as needed. \
The summary replaces the earlier conversation, so it must stand alone; do not \
omit technical details, and when unsure whether something matters, keep it.";

/// Resolve the compaction token threshold from the merged config.
///
/// An absolute token count (`CompactionMode::Tokens`) is used as-is.  A
/// percentage (`CompactionMode::Percent`) is applied to the model's known
/// context window (falling back to `None` — disabled — when the window is
/// unknown).  `None` (no `compaction` set in config) disables compaction.
fn resolve_compaction_threshold(config: &Config) -> Option<usize> {
    use crate::config::CompactionMode;
    match &config.compaction {
        Some(CompactionMode::Tokens(n)) => Some(*n),
        Some(CompactionMode::Percent(pct)) => {
            memory::lookup_context_length(config.provider(), config.model_name())
                .map(|ctx| (ctx as usize) * (*pct as usize) / 100)
        }
        None => None,
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
