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

use crate::config::{self, Config, McpServerDef, merge_session_config, session_mcp_server_names};
use crate::events::{
    EditContent, LogEntry, PromptSource, ServerMessage, SessionEvent, TurnEndReason,
};
use crate::memory::{
    self, LogReplayMemory, TransactionLog, build_session_memory, collect_branch,
    prompt_history_path,
};
use crate::model;
use crate::preamble::build_preamble;
use crate::session_state::{PersistedSessionState, SessionState};
use crate::transport::{PersistedTransport, Transport};
use goop_shared::{SessionConfig, SettingsUpdate};

// ── subscriber with history replay ──────────────────────────────

/// Returned by [`Session::subscribe_all`]. Replays the active branch's
/// history before yielding live events.
///
/// On a [`Reset`](ServerMessage::Reset) (fork), the subscriber re-snapshots
/// the active branch up to the fork point and re-replays it — so clients
/// see the new branch without reconnecting.
pub struct SessionSubscriber {
    /// Shared log, for re-snapshotting the active branch on `Reset`.
    log: Arc<Mutex<TransactionLog>>,
    /// Current history-replay buffer (the active branch, owned).
    history: Vec<LogEntry>,
    history_cursor: usize,
    history_done: bool,
    rx: broadcast::Receiver<ServerMessage>,
}

impl SessionSubscriber {
    /// Wait for the next [`ServerMessage`] (history first, then live).
    ///
    /// After the last history entry, the next call returns
    /// [`HistoryComplete`](ServerMessage::HistoryComplete) to signal the
    /// transition to live events.  A [`Reset`](ServerMessage::Reset) (fork)
    /// triggers a re-snapshot of the active branch: the subscriber refills
    /// its history buffer from the fork point and the *next* calls replay it
    /// again, followed by another `HistoryComplete`.
    pub async fn recv(&mut self) -> Result<ServerMessage, broadcast::error::RecvError> {
        if self.history_cursor < self.history.len() {
            let entry = self.history[self.history_cursor].clone();
            self.history_cursor += 1;
            return Ok(ServerMessage::Entry(entry));
        }
        if !self.history_done {
            self.history_done = true;
            return Ok(ServerMessage::HistoryComplete);
        }
        match self.rx.recv().await {
            // A fork happened at `tip`.  Re-snapshot the active branch up to
            // `tip` (the fork point) so the re-replay excludes the new
            // branch's entries — which may already have been appended to the
            // log by the time we process this.  Yield the Reset so the client
            // clears and re-enters catch-up; the re-replayed history and a
            // fresh HistoryComplete follow.
            Ok(ServerMessage::Reset { tip }) => {
                let log = self.log.lock().await;
                self.history = collect_branch(Some(tip), log.entries());
                self.history_cursor = 0;
                self.history_done = false;
                Ok(ServerMessage::Reset { tip })
            }
            Ok(other) => Ok(other),
            Err(e) => Err(e),
        }
    }
}

// ── session ─────────────────────────────────────────────────────

/// A queued action drained serially by the background worker.
///
/// All prompts and compaction requests go through this channel so only one
/// action runs at a time — no races with `drain_queue`, and every action
/// emits proper lifecycle events (`Thinking` + `TurnEnded`) so the client can
/// show progress and cancel.
enum QueuedAction {
    /// Submit a text prompt.
    Prompt {
        content: String,
        source: PromptSource,
        /// Seq of the fork point (the parent of the target), or `None` for a
        /// normal linear prompt.
        fork_point: Option<u64>,
        /// Fires when the turn completes.
        done: Option<oneshot::Sender<()>>,
    },
    /// Manually compact a range of agent-visible messages into a summary.
    CompactRange {
        covers: Vec<u64>,
        /// Fires when the compaction finishes (success or failure).
        done: Option<oneshot::Sender<()>>,
    },
    /// Change the session settings mid-conversation.  The server merges
    /// the overrides, persists, appends `SettingsChanged` to the log, and
    /// rebuilds the agent if needed.
    ApplySettings {
        update: goop_shared::SettingsUpdate,
        /// Fires when the change completes (or fails).
        done: Option<oneshot::Sender<()>>,
    },
}

/// Holds the agent, conversation state, and a serialised prompt queue.
///
/// Multiple views can submit prompts and subscribe to events
/// concurrently — the session guarantees prompts are processed
/// one at a time in FIFO order.
pub struct Session {
    /// Session name (user-supplied or auto-generated like `20260128_001`).
    name: String,
    /// AGENTS.md content most recently baked into the live agent's preamble.
    /// `None` means no AGENTS.md was found.  Compared with the current CWD's
    /// AGENTS.md after each turn to detect when `cd` / `ssh` / `disconnect`
    /// changes the relevant project context.
    last_agents_md: std::sync::Mutex<Option<String>>,
    /// Shared mutable state (CWD, transport, home_dir) accessible by tools.
    ///
    /// Held here to keep the `Arc` alive; tools receive their own clone
    /// at construction time via [`model::build_agent`].
    #[allow(dead_code)]
    pub(crate) state: Arc<SessionState>,
    /// The active agent — wrapped in a [`Mutex`] so it can be swapped
    /// mid-session when the model changes.  The lock is never contended:
    /// all access is serialized through [`drain_queue`](Self::drain_queue).
    agent: std::sync::Mutex<Arc<crate::model::AnyAgent>>,
    tx: broadcast::Sender<ServerMessage>,
    history: Arc<Mutex<TransactionLog>>,

    /// Clone of the log-replay memory, used to estimate token counts for
    /// the context-usage progress bar.  Shares the same transaction-log
    /// `Arc<Mutex<TransactionLog>>` as the agent's memory, so it always
    /// reflects the latest conversation state.
    memory: LogReplayMemory,

    /// Context window limit (in tokens) for the progress bar.  Uses the
    /// compaction budget when compaction is enabled, otherwise the model's
    /// known context window.  Wrapped in a [`Mutex`] so
    /// [`apply_settings`](Self::apply_settings) can update it through `&self`.
    context_limit: std::sync::Mutex<Option<usize>>,

    /// Token budget at which the agent-visible conversation is compacted into a
    /// rolling LLM summary (see [`maybe_compact`](Self::maybe_compact)).
    /// `None` disables compaction (unlimited context).  Wrapped in a [`Mutex`]
    /// so [`apply_settings`](Self::apply_settings) can update it through `&self`.
    compaction_threshold: std::sync::Mutex<Option<usize>>,

    /// The session's model string (e.g. `deepseek/deepseek-v4-pro`), recorded
    /// recorded in `Compacted` and `SettingsChanged` events.  Wrapped in a
    /// [`apply_settings`](Self::apply_settings) can update it through `&self`.
    model_label: std::sync::Mutex<String>,

    /// Tool-pair summarization configuration.  When `enabled` is false, the
    /// summarizer is a no-op.
    tool_summarization: crate::config::ToolSummarizationConfig,

    /// Separate agent for tool-pair summarization (built from
    /// `tool_summarization.model`).  Wrapped in a [`Mutex`] so it can
    /// be swapped when the model changes.  `None` when no separate model
    /// is configured — the session's main `agent` is used instead.
    tool_summarizer: std::sync::Mutex<Option<Arc<crate::model::AnyAgent>>>,

    /// Push a prompt here from any view; the background worker drains it.
    /// See [`QueuedAction`] for the variants.
    submit_tx: mpsc::UnboundedSender<QueuedAction>,

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

    /// Cached MCP managers, stored so model/tool reconfiguration can
    /// reconstruct the agent without reconnecting to MCP servers.
    shared_mcp: Arc<crate::mcp::McpManager>,

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
    /// `initial_cwd` is only used when creating a brand-new session
    /// (no persisted state); on resume the persisted CWD takes precedence.
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
        initial_cwd: Option<String>,
    ) -> anyhow::Result<Arc<Self>> {
        // ── persistence paths ──────────────────────────────────
        let name = session_name.unwrap_or_else(next_session_name);
        let state_path = crate::session_state::state_path(&name);

        // ── load persisted state (config overrides + CWD + transport) ──
        let mut persisted = PersistedSessionState::load(&name).unwrap_or_default();

        // If the caller requested a specific initial CWD and this session
        // has no persisted state (brand new), override the default CWD.
        if !state_path.exists() {
            if let Some(ref cwd) = initial_cwd {
                let resolved = resolve_initial_cwd(cwd);
                persisted.local_cwd = resolved;
            }
        }

        // Merge session config overrides into the global config.
        let merged_config = merge_session_config(&persisted.config, config);

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

        // ── MCP servers ────────────────────────────────────────
        // Resolve which servers to enable: global ∪ session overrides.
        let session_names = crate::mcp::resolve(config, session_mcp_server_names(&persisted.config));

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
        let mut log = TransactionLog::open(&name).await?;
        // Stamp the creation model into SessionInfo for new sessions
        // (where it was injected as None) so the log records what model
        // the session started with.  Idempotent on resume.
        log.stamp_session_info_model(&merged_config.model.to_string());

        // Apply a persisted active tip (a resumed session that was viewing an
        // older branch — future UI).  `None` (the default) means "last entry"
        // — linear, or the newest fork (basic forking always continues on the
        // newest branch, whose entries are appended last, so the default is
        // correct without any persistence).
        if let Some(tip) = persisted.active_tip {
            log.set_active_tip(tip);
            state.set_active_tip(Some(tip)).await;
        }
        // Broadcast SessionInfo immediately so live subscribers see it.
        // (The log already has it as the first entry; this is the delivery
        // mechanism for live-only subscribers.)
        if let Some(info) = log.entries().first() {
            let _ = tx.send(ServerMessage::Entry(info.clone()));
        }

        // ── system prompt (preamble) ────────────────────────────────
        // The log is the source of truth.  For a new/legacy session the
        // preamble is built from the template + env context and injected; for
        // a resumed session the stored value is authoritative (the preamble is
        // NOT rebuilt — the log records exactly what the agent saw).
        let preamble = match log.system_prompt() {
            Some(stored) => {
                tracing::debug!("session {name}: using stored system prompt");
                stored
            }
            None => {
                let built = build_preamble(
                    &initial_cwd_for_preamble.display().to_string(),
                    &config.home_dir,
                );
                log.ensure_system_prompt(&built).await;
                built
            }
        };

        // ── scan: if the merged config (global + session overrides) differs
        //    from the last `SettingsChanged` recorded in the log (e.g. someone
        //    edited <name>.state.toml between sessions), append a
        //    `SettingsChanged` event so the audit trail is complete.
        let history = Arc::new(Mutex::new(log));
        {
            let log_settings = history.lock().await.last_settings_in_log();
            if persisted.config != log_settings {
                let mut log = history.lock().await;
                let entry = log
                    .append(SessionEvent::SettingsChanged {
                        config: persisted.config.clone(),
                    })
                    .await;
                let _ = tx.send(ServerMessage::Entry(entry));
            }
        }

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

        // Build a separate summarizer agent if a tool-summarization model is
        // configured; otherwise fall back to the main agent.
        let tool_summarizer = match model::build_summarizer(&merged_config) {
            Ok(Some(a)) => Some(a),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    "failed to build tool summarizer agent, falling back to main model: {e}"
                );
                None
            }
        };

        let tool_summarization = merged_config.tool_summarization.clone();

        let agent = model::build_agent(&merged_config, &preamble, mem, state.clone(), mcp_tools)?;

        // Snapshot the AGENTS.md content baked into the preamble, so we can
        // detect when cd/ssh/disconnect changes the relevant project context.
        let last_agents_md = state.read_cwd_file("AGENTS.md").await;

        let this = Arc::new(Self {
            name: name.clone(),
            state: state.clone(),
            agent: std::sync::Mutex::new(agent),
            last_agents_md: std::sync::Mutex::new(last_agents_md),
            tx,
            history,
            memory: memory_for_usage,
            context_limit: std::sync::Mutex::new(context_limit),
            compaction_threshold: std::sync::Mutex::new(compaction_threshold),
            model_label: std::sync::Mutex::new(model_label),
            tool_summarization,
            tool_summarizer: std::sync::Mutex::new(tool_summarizer),
            submit_tx,
            cancel_tx: Mutex::new(None),
            is_running: AtomicBool::new(false),
            session_mcp: session_mcp_manager,
            shared_mcp: Arc::clone(&shared_mcp_manager),
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
        // Unbounded send never fails.  No fork point → linear append.
        let _ = self.submit_tx.send(QueuedAction::Prompt {
            content: prompt.into(),
            source,
            fork_point: None,
            done: Some(tx),
        });
        rx
    }

    /// Fork the conversation from the point *before* `target` (i.e. from
    /// `target`'s parent) and regenerate: a new `UserPrompt` carrying
    /// `content` is appended with `parent` set to that fork point, the active
    /// tip moves to the new branch, and a turn runs.  The old branch is
    /// preserved in the append-only log.  See §2.9 of the redesign doc.
    ///
    /// This is edit-and-regenerate (like ChatGPT): the user edits a past user
    /// message and resubmits.  Unlike [`edit`](Self::edit) (which overlays the
    /// change in place without branching), forking creates a new branch and
    /// reruns the turn.
    ///
    /// The fork point is computed now (from the log) but applied in
    /// [`drain_queue`](Self::drain_queue) when the prompt is processed, so it
    /// is atomic with respect to other queued prompts.
    pub async fn fork(&self, target: u64, content: String) {
        // The fork point is the parent of the target event — branching from
        // there excludes the target and everything after it on the old branch.
        let fork_point = {
            let log = self.history.lock().await;
            log.entries()
                .iter()
                .find(|e| e.seq == target)
                .and_then(|e| e.parent)
        };
        let Some(fork_point) = fork_point else {
            tracing::warn!("fork: target seq {target} not found or has no parent");
            return;
        };
        tracing::info!("forking from seq {fork_point} (target {target} replaced)");
        let (tx, _rx) = oneshot::channel();
        let _ = self.submit_tx.send(QueuedAction::Prompt {
            content,
            source: PromptSource::Web,
            fork_point: Some(fork_point),
            done: Some(tx),
        });
    }

    /// Cancel the currently-running LLM turn (if any).
    /// Safe to call from any async context; idempotent.
    pub async fn cancel(&self) {
        if let Some(tx) = self.cancel_tx.lock().await.take() {
            let _ = tx.send(());
        }
    }

    /// Edit the content of a prior event (`target` seq) in the agent's view.
    ///
    /// Appends an [`SessionEvent::Edited`] overlay.  The original event stays
    /// in the log untouched; replay uses the replacement for the agent-visible
    /// set, so the LLM sees the edited content on its next call.  This is
    /// "writing into the LLM's mind" — see §2.10 of the redesign doc.
    pub async fn edit(&self, target: u64, replacement: EditContent) {
        self.emit(SessionEvent::Edited {
            target,
            replacement,
        })
        .await;
    }

    /// Hide a prior event (`target` seq) from the agent's view.
    ///
    /// Appends a [`SessionEvent::Deleted`] overlay.  If `target` is one half
    /// of a tool call+result pair, the matching half is also deleted (a second
    /// `Deleted` event) so the agent never sees an orphaned call or result —
    /// which some provider APIs reject.  The originals stay in the log;
    /// replay skips deleted events.
    pub async fn delete(&self, target: u64) {
        let other_half = self.resolve_tool_pair(target).await;
        self.emit(SessionEvent::Deleted { target }).await;
        if let Some(other) = other_half {
            self.emit(SessionEvent::Deleted { target: other }).await;
        }
    }

    /// If `target` is a `ToolCall`/`ToolResult`, find the seq of the matching
    /// other half (the event with the same tool-call id and a different seq).
    /// Returns `None` for non-tool events or when no match is found.
    async fn resolve_tool_pair(&self, target: u64) -> Option<u64> {
        let log = self.history.lock().await;
        let entries = log.entries();
        let target_entry = entries.iter().find(|e| e.seq == target)?;
        let id = match &target_entry.event {
            SessionEvent::ToolCall { id, .. } | SessionEvent::ToolResult { id, .. } => id,
            _ => return None,
        };
        entries
            .iter()
            .find(|e| {
                e.seq != target
                    && matches!(
                        &e.event,
                        SessionEvent::ToolCall { id: eid, .. }
                        | SessionEvent::ToolResult { id: eid, .. }
                            if eid == id
                    )
            })
            .map(|e| e.seq)
    }

    /// Manually compact a range of agent-visible messages into a summary.
    /// The user selects messages in the UI; `covers` is their seqs.  The
    /// action is queued and processed by [`drain_queue`](Self::drain_queue)
    /// serially — no race with prompts, and lifecycle events (`Thinking` +
    /// `TurnEnded`) bracket the summarization so the client can show progress
    /// and cancel.
    ///
    /// Returns a receiver that fires when the compaction completes (or fails).
    /// See §2.11 of the redesign doc.
    ///
    /// Unlike [`maybe_compact`](Self::maybe_compact) (which covers the entire
    /// agent-visible prefix), manual compaction covers only the selected
    /// range — messages before and after the range remain agent-visible.
    /// Replay is the same either way: `covers` is an arbitrary `Vec<u64>`, so
    /// no special-casing is needed.
    pub fn compact_range(&self, covers: Vec<u64>) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.submit_tx.send(QueuedAction::CompactRange {
            covers,
            done: Some(tx),
        });
        rx
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
    pub fn subscribe(&self) -> broadcast::Receiver<ServerMessage> {
        self.tx.subscribe()
    }

    /// Subscribe with **full history replay** of the active branch.
    ///
    /// Late-joining views (web, phone, …) receive every event on the active
    /// branch (walked from the active tip) before transitioning to live
    /// events.  On a later fork, the subscriber re-replays the new branch
    /// automatically (see [`SessionSubscriber`]).
    pub async fn subscribe_all(&self) -> SessionSubscriber {
        let (history, rx) = {
            let log = self.history.lock().await;
            let history = collect_branch(log.active_tip(), log.entries());
            let rx = self.tx.subscribe();
            (history, rx)
        };
        SessionSubscriber {
            log: self.history.clone(),
            history,
            history_cursor: 0,
            history_done: false,
            rx,
        }
    }

    // ── internals ────────────────────────────────────────────────

    /// Background worker: drain prompts and compaction requests one at a time.
    async fn drain_queue(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<QueuedAction>) {
        while let Some(action) = rx.recv().await {
            match action {
                QueuedAction::Prompt {
                    content,
                    source,
                    fork_point,
                    done,
                } => {
                    // Write every prompt to the global history file (all sources).
                    append_prompt_to_history(&content).await;

                    // ── fork (edit-and-regenerate) ──
                    if let Some(tip) = fork_point {
                        let mut log = self.history.lock().await;
                        log.set_active_tip(tip);
                        drop(log);
                        let _ = self.tx.send(ServerMessage::Reset { tip });
                    }

                    // Auto-compaction (tier 2) — roll up the prefix if over budget.
                    self.maybe_compact().await;

                    // Tool-pair summarization (tier 1) — summarise verbose pairs.
                    self.maybe_summarize_tool_pairs().await;

                    self.emit(SessionEvent::UserPrompt {
                        content: content.clone(),
                        source,
                    })
                    .await;
                    self.run_one(&content).await;

                    // cd / ssh / disconnect may have changed the project
                    // context — rebuild the preamble if AGENTS.md differs.
                    self.maybe_rebuild_preamble().await;

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
                QueuedAction::CompactRange { covers, done } => {
                    self.compact_range_impl(covers).await;
                    if let Some(tx) = done {
                        let _ = tx.send(());
                    }
                }
                QueuedAction::ApplySettings { update, done } => {
                    self.apply_settings(update).await;
                    if let Some(tx) = done {
                        let _ = tx.send(());
                    }
                }
            }
        }
    }

    /// Actual compaction logic, called from [`drain_queue`](Self::drain_queue).
    ///
    /// Emits `Thinking` → `Compacted` (or `Error`) → `TurnEnded` so the
    /// client sees a normal turn lifecycle: the input button goes to Running
    /// mode (showing Cancel), and the message log shows a thinking placeholder
    /// replaced by the result.
    async fn compact_range_impl(&self, covers: Vec<u64>) {
        if covers.len() < 2 {
            tracing::warn!("compact_range: covers too short ({})", covers.len());
            self.emit(SessionEvent::TurnEnded {
                reason: TurnEndReason::Error {
                    message: "Select at least 2 messages to compact.".into(),
                },
            })
            .await;
            return;
        }

        let items = self.memory.agent_visible_items().await;
        let messages = memory::covered_messages(&items, &covers);
        if messages.is_empty() {
            // The covers didn't match any agent-visible items.  This is a
            // client/server mismatch — the client's displayed message list
            // got out of sync with the server's agent-visible projection.
            tracing::warn!(
                "compact_range: no agent-visible items matched covers={covers:?}; \
                 agent-visible seqs: {:?}",
                items.iter().map(|i| i.seq).collect::<Vec<_>>()
            );
            self.emit(SessionEvent::TurnEnded {
                reason: TurnEndReason::Error {
                    message: "The selected messages are no longer agent-visible. \
                              They may have been compacted or deleted by a prior action."
                        .into(),
                },
            })
            .await;
            return;
        }

        tracing::info!(
            "manual compaction of {} messages ({} covers, {} agent-visible items)",
            messages.len(),
            covers.len(),
            items.len()
        );

        // Emit Thinking so the client enters the "running" state (Cancel
        // button visible).  We do this *after* validation so the thinking
        // placeholder doesn't flash for an immediate error.
        self.emit(SessionEvent::Thinking).await;

        // Set up a cancellation channel for this compaction so the user can
        // abort a long-running summarization via the Cancel button.
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        {
            let mut guard = self.cancel_tx.lock().await;
            *guard = Some(cancel_tx);
        }

        // Choose the summarization path.  Tool-only ranges (no user text, no
        // assistant text) get the lighter multi-tool prompt — shorter system
        // instruction, tool text as the user message.  Mixed ranges get the
        // full compaction prompt with the transcript embedded in the system
        // prompt.
        let tool_only = is_tool_only(&messages);
        let (system_prompt, user_prompt): (String, String) = if tool_only {
            let messages_text = memory::format_messages_for_compacting(&messages);
            (
                MULTI_TOOL_COMPACT_PROMPT.to_string(),
                format!("Summarize these tool interactions:\n\n{messages_text}"),
            )
        } else {
            let messages_text = memory::format_messages_for_compacting(&messages);
            (
                COMPACTION_SYSTEM_PROMPT.replace("{{ messages }}", &messages_text),
                "Please summarize the conversation history provided in the system prompt."
                    .to_string(),
            )
        };

        tracing::info!(
            "manual compaction: {} messages, tool_only={tool_only}",
            messages.len()
        );

        // Race the summarization against cancellation.
        // Clone the Arc out of the Mutex so the guard doesn't cross .await.
        let agent = self.agent.lock().unwrap().clone();
        let outcome = tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                // User cancelled — drop the in-flight summarization.
                tracing::info!("manual compaction cancelled");
                None
            }
            result = agent.summarize(system_prompt, &user_prompt) => {
                Some(result)
            }
        };

        // Always clear the cancel channel.
        self.cancel_tx.lock().await.take();

        match outcome {
            None => {
                // Cancelled.
                self.emit(SessionEvent::TurnEnded {
                    reason: TurnEndReason::Cancelled { prompt: None },
                })
                .await;
            }
            Some(Ok(summary)) => {
                // Append a continuation instruction so the agent knows it's
                // working from a summary and doesn't mention the compaction.
                let summary_with_continuation = format!(
                    "{}\n\n{M}",
                    summary.trim_end(),
                    M = MANUAL_COMPACT_CONTINUATION_TEXT,
                );
                let model = self.model_label.lock().unwrap().clone();
                self.emit(SessionEvent::Compacted {
                    summary: summary_with_continuation,
                    model,
                    covers,
                    manual: true,
                })
                .await;
                self.emit(SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                })
                .await;
            }
            Some(Err(e)) => {
                tracing::warn!("manual compaction summarization failed: {e:#}");
                self.emit(SessionEvent::TurnEnded {
                    reason: TurnEndReason::Error {
                        message: format!("Compaction summarization failed: {e}"),
                    },
                })
                .await;
            }
        }
    }

    /// Apply session config overrides mid-conversation: merge incoming
    /// [`SettingsUpdate`] into the current `SessionConfig`, persist,
    /// append `SettingsChanged`, and rebuild agent if needed.
    async fn apply_settings(&self, incoming: SettingsUpdate) {
        let current = self.state.session_config().await;

        // Build the new config by applying each incoming `Setting<T>` to the
        // current override.  `Set(v)` replaces; `Clear` removes; `None` leaves.
        let mut merged = current.clone();
        apply_setting_field(&mut merged.model, incoming.model);
        apply_setting_field(&mut merged.ollama_base_url, incoming.ollama_base_url);
        apply_setting_field(
            &mut merged.enabled_tool_groups,
            incoming.enabled_tool_groups,
        );

        if merged == current {
            tracing::info!("settings unchanged, skipping rebuild");
            return;
        }

        let model_changed = merged.model != current.model;
        let tools_changed = merged.enabled_tool_groups != current.enabled_tool_groups;
        let url_changed = merged.ollama_base_url != current.ollama_base_url;

        // Persist to disk so the change survives restarts.
        self.state.set_session_config(merged.clone()).await;

        // Emit the audit event.
        self.emit(SessionEvent::SettingsChanged {
            config: merged.clone(),
        })
        .await;

        // Only rebuild the agent if a provider-affecting field changed.
        if !model_changed && !tools_changed && !url_changed {
            tracing::info!(
                "settings changed (non-provider fields only), skipping agent rebuild"
            );
            return;
        }

        let model_str = merged.model.as_deref().unwrap_or("unknown");
        let new_model: crate::config::Model = match model_str.parse() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("apply_settings: invalid model {model_str:?}: {e}");
                return;
            }
        };

        let provider = new_model.provider();
        let model_name = new_model.model_name().to_string();

        tracing::info!(
            "rebuilding agent with {model_str} ({}, {model_name})",
            provider.label()
        );

        let temp_config = build_temp_config(&merged);

        let preamble = {
            let log = self.history.lock().await;
            log.system_prompt().unwrap_or_default()
        };

        let mut mcp_tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        mcp_tools.extend(self.shared_mcp.build_tools());
        mcp_tools.extend(self.session_mcp.build_tools());

        let new_agent = match model::build_agent(
            &temp_config,
            &preamble,
            build_session_memory(self.history.clone()),
            self.state.clone(),
            mcp_tools,
        ) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("apply_settings: failed to build agent: {e}");
                return;
            }
        };

        let new_summarizer = match model::build_summarizer(&temp_config) {
            Ok(opt) => opt,
            Err(e) => {
                tracing::warn!("apply_settings: failed to build summarizer: {e}");
                None
            }
        };

        let new_context_limit =
            crate::memory::lookup_context_length(provider, &model_name)
                .map(|v| v as usize);

        let new_compaction_threshold =
            if let Some(ref mode) = temp_config.compaction {
                match mode {
                    goop_shared::CompactionMode::Tokens(n) => Some(*n),
                    goop_shared::CompactionMode::Percent(pct) => {
                        crate::memory::lookup_context_length(provider, &model_name)
                            .map(|ctx| (ctx as usize) * (*pct as usize) / 100)
                    }
                }
            } else {
                None
            };

        *self.agent.lock().unwrap() = new_agent;
        *self.tool_summarizer.lock().unwrap() = new_summarizer;
        *self.model_label.lock().unwrap() = model_str.to_string();
        *self.context_limit.lock().unwrap() = new_context_limit;
        *self.compaction_threshold.lock().unwrap() = new_compaction_threshold;

        tracing::info!("agent rebuilt with new settings");
    }

    /// Queue a settings change.  Returns a receiver that fires when done.
    pub fn apply_session_settings(&self, update: SettingsUpdate) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.submit_tx.send(QueuedAction::ApplySettings {
            update,
            done: Some(tx),
        });
        rx
    }

    /// If the current CWD has a different AGENTS.md than what's baked into
    /// the agent's preamble (because the user `cd`'d, `ssh`'d, or
    /// `disconnect`ed to a different project), rebuild the preamble and
    /// agent so the LLM sees the new project context.
    async fn maybe_rebuild_preamble(&self) {
        let current_agents_md = self.state.read_cwd_file("AGENTS.md").await;

        {
            let last = self.last_agents_md.lock().unwrap();
            if *last == current_agents_md {
                return; // unchanged — nothing to do
            }
        }

        let cwd = self.state.cwd_display().await;
        let home_dir = self.state.home_dir();
        let new_preamble = crate::preamble::build_preamble_with(
            &cwd,
            &home_dir,
            current_agents_md.as_deref(),
        );

        // Check whether the rendered preamble actually changed.
        // (AGENTS.md might be None→None but CWD changed → rendered
        // preamble is different.  Or AGENTS.md content changed but the
        // rest is stable.  Comparing the final string handles both.)
        let old_preamble = {
            let log = self.history.lock().await;
            log.system_prompt().unwrap_or_default()
        };
        if new_preamble == old_preamble {
            // CWD changed but preamble is identical (e.g. two dirs with
            // the same AGENTS.md).  Still update the tracker so we don't
            // keep re-reading the file on every turn.
            *self.last_agents_md.lock().unwrap() = current_agents_md;
            return;
        }

        tracing::info!(
            "preamble changed — AGENTS.md {} → rebuilding agent",
            if current_agents_md.is_some() { "added/updated" } else { "removed" }
        );

        // Append the new SystemPrompt to the transaction log for audit.
        self.emit(SessionEvent::SystemPrompt {
            content: new_preamble.clone(),
        })
        .await;

        // ── rebuild agent with the new preamble ─────────────────
        // Same pattern as apply_settings, but we keep the same model +
        // tools — only the preamble changes.
        let session_cfg = self.state.session_config().await;
        let merged_config = build_temp_config(&session_cfg);

        let mut mcp_tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        mcp_tools.extend(self.shared_mcp.build_tools());
        mcp_tools.extend(self.session_mcp.build_tools());

        let new_agent = match crate::model::build_agent(
            &merged_config,
            &new_preamble,
            crate::memory::build_session_memory(self.history.clone()),
            self.state.clone(),
            mcp_tools,
        ) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("maybe_rebuild_preamble: failed to build agent: {e}");
                return;
            }
        };

        // Update the tracker and swap the agent.
        *self.last_agents_md.lock().unwrap() = current_agents_md;
        *self.agent.lock().unwrap() = new_agent;
    }

    // ── helpers ────────────────────────────────────────────────────

    /// If the agent-visible conversation has grown past the compaction
    /// threshold, summarize it into a rolling `Compacted` event so the next
    /// turn stays within the context budget.  The entire agent-visible prefix
    /// is covered; the in-progress prompt (handled by rig) is preserved.
    /// Summaries are themselves agent-visible, so later compactions summarize
    /// the prior summary — a rolling summary.
    ///
    /// **Most-recent user message preservation:** like goose, the most-recent
    /// user message is kept uncompacted so the agent still sees the exact
    /// user request.  It is *included* in the summarization context (the LLM
    /// should describe it) but *excluded* from `covers` (replay leaves it in
    /// place after the summary).
    ///
    /// No-op when compaction is disabled (`threshold == None`) or the
    /// conversation is still small.
    async fn maybe_compact(&self) {
        let Some(threshold) = *self.compaction_threshold.lock().unwrap() else {
            return;
        };
        let items = self.memory.agent_visible_items().await;
        let messages: Vec<Message> = items.iter().map(|i| i.msg.clone()).collect();
        let tokens = self.memory.count_tokens(&messages);
        let Some(mut covers) = memory::compaction_covers(&items, threshold, tokens) else {
            return;
        };

        // Find the most-recent user message so we can preserve it
        // uncompacted (goose-style).  Including it in summarization but
        // excluding from covers means: the LLM describes it in the summary,
        // then the original survives beneath the summary for the next turn.
        let last_user_seq: Option<u64> = items
            .iter()
            .rev()
            .find(|i| matches!(&i.msg, Message::User { .. }))
            .map(|i| i.seq);

        if let Some(seq) = last_user_seq {
            covers.retain(|&s| s != seq);
            // Don't compact if the only thing left would be the user message
            // (covers would be empty or just 1 item — not worth it).
            if covers.len() < 2 {
                return;
            }
        }

        tracing::info!(
            "compacting {} agent-visible items (~{} tokens >= threshold {threshold}); \
             covers={} items, most-recent user msg {}",
            items.len(),
            tokens,
            covers.len(),
            if last_user_seq.is_some() { "preserved" } else { "none" }
        );

        // Format the conversation as text embedded in the system prompt so
        // the LLM reads a transcript rather than acting as a participant.
        let messages_text = memory::format_messages_for_compacting(&messages);
        let system_prompt =
            COMPACTION_SYSTEM_PROMPT.replace("{{ messages }}", &messages_text);
        let user_prompt =
            "Please summarize the conversation history provided in the system prompt.";

        match { self.agent.lock().unwrap().clone() }.summarize(system_prompt, user_prompt).await {
            Ok(summary) => {
                // Append a continuation instruction so the agent knows it's
                // working from a summary and doesn't mention the compaction.
                let summary_with_continuation = format!(
                    "{}\n\n{C}",
                    summary.trim_end(),
                    C = AUTO_COMPACT_CONTINUATION_TEXT,
                );
                let model = self.model_label.lock().unwrap().clone();
                self.emit(SessionEvent::Compacted {
                    summary: summary_with_continuation,
                    model,
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

    /// Summarize verbose tool call+result pairs into short `ToolSummarized`
    /// events, reclaiming tokens incrementally (tier-1 compaction).
    ///
    /// Runs **between prompts** in `drain_queue` (alongside `maybe_compact`),
    /// not during streaming.  The log lock is held only for the fast snapshot
    /// and commit steps; the slow LLM summarization happens outside the lock.
    /// See §5.3 of the redesign doc.
    ///
    /// No-op when tool-pair summarization is disabled, the tool-call count
    /// is below the trigger, or there are no qualifying candidate pairs.
    async fn maybe_summarize_tool_pairs(&self) {
        if !self.tool_summarization.enabled {
            return;
        }

        // ── 1. Snapshot candidates ──
        // `agent_visible_items` returns an owned snapshot (the log lock is
        // released on return), so the pure selection logic runs lock-free.
        let items = self.memory.agent_visible_items().await;
        let candidates =
            memory::select_tool_summary_candidates(&items, &self.tool_summarization, |m| {
                self.memory.count_tokens(m)
            });
        if candidates.is_empty() {
            return;
        }

        // ── 2. Summarize (outside the lock) ──
        let summarizer_arc: Arc<crate::model::AnyAgent> = {
            let s_guard = self.tool_summarizer.lock().unwrap();
            let a_guard = self.agent.lock().unwrap();
            s_guard
                .clone()
                .unwrap_or_else(|| a_guard.clone())
        };

        let mut summaries: Vec<(String, String)> = Vec::new();
        for c in &candidates {
            // Goose-style: the formatted pair text goes in the user message,
            // the system prompt is a short generic instruction.
            let pair_text =
                memory::format_messages_for_compacting(&[c.call_msg.clone(), c.result_msg.clone()]);
            let user_prompt = format!(
                "Summarize this tool interaction:\n\n{}",
                pair_text
            );
            match summarizer_arc
                .summarize(TOOL_PAIR_SYSTEM_PROMPT.to_string(), &user_prompt)
                .await
            {
                Ok(summary) => summaries.push((c.id.clone(), summary)),
                Err(e) => {
                    tracing::warn!("tool-pair summarization failed for {}: {e:#}", c.id);
                }
            }
        }

        if summaries.is_empty() {
            return;
        }

        // ── 3. Commit (with re-validation) ──
        // Re-acquire the snapshot and confirm each id is still agent-visible.
        // The conversation is serial (drain_queue), but this is defence in
        // depth for future concurrency — see §5.3 step 3.
        let items = self.memory.agent_visible_items().await;
        let validated = memory::revalidate_tool_summaries(&items, &summaries);
        let model = self.model_label.lock().unwrap().clone();
        for (id, summary) in &validated {
            self.emit(SessionEvent::ToolSummarized {
                id: id.clone(),
                summary: summary.clone(),
                model: model.clone(),
            })
            .await;
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
            let model = self.model_label.lock().unwrap().clone();
            self.emit(SessionEvent::ContextSnapshot {
                seqs: items.iter().map(|i| i.seq).collect(),
                model,
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
            let agent = self.agent.lock().unwrap().clone();
            let mut stream = agent.stream_prompt(prompt).await;

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
        let Some(limit) = *self.context_limit.lock().unwrap() else {
            return;
        };
        let used = self.memory.estimated_tokens().await;
        self.emit(SessionEvent::ContextUsage { used, limit }).await;
    }

    /// Send an event to live subscribers and append it to the transaction log
    /// (which persists to disk atomically).
    ///
    /// `TransactionLog::append` assigns the next monotonic `seq`, computes
    /// `parent` from the active tip, stamps `ts`, pushes to memory, and writes
    /// the JSONL line to disk — all under the history lock, so seq order,
    /// parent-pointer order, and file-append order can never diverge.  Live
    /// subscribers receive the full [`LogEntry`] envelope (wrapped in
    /// [`ServerMessage::Entry`]) over the WebSocket; the envelope carries the
    /// `seq` and `parent` the client needs for overlay targeting and fork
    /// detection.
    ///
    /// The append and broadcast stay under the lock deliberately:
    /// `subscribe_all` does `lock → snapshot active branch → subscribe to tx`,
    /// so keeping `append + send` atomic with respect to that ensures every
    /// subscriber sees each event exactly once (history XOR live — never
    /// both, never neither).
    async fn emit(&self, event: SessionEvent) {
        let mut log = self.history.lock().await;
        let entry = log.append(event).await;
        let _ = self.tx.send(ServerMessage::Entry(entry));
    }

    // ── test support ───────────────────────────────────────────────────

    /// Build a minimal session backed by a mock agent and an in-memory
    /// transaction log.  Only the fields needed for compaction testing
    /// (`agent`, `memory`, `history`, `compaction_threshold`, `model_label`,
    /// `tx`) are populated; everything else is a no-op sentinel.
    ///
    /// The returned `(Session, broadcast::Receiver)` pair lets the test
    /// drive compaction (`maybe_compact` / `compact_range`) and observe the
    /// emitted events.
    #[cfg(test)]
    pub(crate) async fn for_compaction_test(
        name: &str,
        summarize_result: &str,
        compaction_threshold: usize,
    ) -> (Arc<Self>, broadcast::Receiver<ServerMessage>) {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransactionLog::open_at(tmp.path().join("test.jsonl"), name)
            .await
            .unwrap();
        // Keep tempdir alive for the session's lifetime.
        std::mem::forget(tmp);

        let history = Arc::new(Mutex::new(log));
        let memory = LogReplayMemory::new(history.clone());
        let (tx, rx) = broadcast::channel(256);

        let session = Arc::new(Self {
            name: name.to_string(),
            last_agents_md: std::sync::Mutex::new(None),
            state: Arc::new(SessionState::new(
                PathBuf::from("/tmp"),
                PathBuf::from("/tmp"),
                Default::default(),
                PathBuf::from("/tmp/test.state.toml"),
            )),
            agent: std::sync::Mutex::new(Arc::new(model::AnyAgent::Mock {
                summarize_result: summarize_result.to_string(),
            })),
            tx: tx.clone(),
            history,
            memory,
            context_limit: std::sync::Mutex::new(None),
            compaction_threshold: std::sync::Mutex::new(Some(compaction_threshold)),
            model_label: std::sync::Mutex::new("mock/model".to_string()),
            tool_summarization: Default::default(),
            tool_summarizer: std::sync::Mutex::new(None),
            submit_tx: mpsc::unbounded_channel().0,
            cancel_tx: Mutex::new(None),
            is_running: AtomicBool::new(false),
            session_mcp: crate::mcp::McpManager::empty(),
            shared_mcp: crate::mcp::McpManager::empty(),
            push_notifier: Arc::new(crate::push::PushManager::new()),
            stt: None,
        });

        (session, rx)
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
    ///
    /// `initial_cwd` is only used when creating a brand-new session
    /// (no persisted state); on resume the persisted CWD takes precedence.
    pub async fn get_or_create(
        &self,
        name: String,
        initial_cwd: Option<String>,
    ) -> anyhow::Result<Arc<Session>> {
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
            initial_cwd,
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
    pub async fn create(
        &self,
        name: Option<String>,
        initial_cwd: Option<String>,
    ) -> anyhow::Result<Arc<Session>> {
        let name = name.unwrap_or_else(next_session_name);
        self.get_or_create(name, initial_cwd).await
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
            let _ = self.get_or_create(name, None).await;
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

/// Resolve a user-provided initial CWD string to a [`PathBuf`].
///
/// Handles `~` for home, `~user` for another user's home, and relative
/// paths (resolved against the process CWD).
fn resolve_initial_cwd(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_cwd();
    }
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(default_cwd);
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if rest.is_empty() {
            return dirs::home_dir().unwrap_or_else(default_cwd);
        }
        let home = dirs::home_dir().unwrap_or_else(default_cwd);
        return home.join(rest);
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        // Relative — resolve against process CWD.
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// Default CWD for a new session — the process's current directory.
fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ── settings helpers ───────────────────────────────────────────────

/// Apply a single [`Setting<T>`] change to a field: `Set(v)` replaces,
/// `Clear` removes the override, `None` leaves it alone.
fn apply_setting_field<T>(target: &mut Option<T>, incoming: Option<goop_shared::Setting<T>>) {
    match incoming {
        Some(goop_shared::Setting::Set(v)) => *target = Some(v),
        Some(goop_shared::Setting::Clear) => *target = None,
        None => {}
    }
}

/// Build a minimal [`Config`] from a [`SessionConfig`] — just enough
/// fields to construct an agent.
fn build_temp_config(session: &SessionConfig) -> Config {
    let mut c = Config::default();
    c.home_dir = crate::config::home_dir();
    if let Some(ref m) = session.model {
        if let Ok(parsed) = m.parse::<crate::config::Model>() {
            c.model = parsed;
        }
    }
    if let Some(t) = session.max_tokens {
        c.max_tokens = t;
    }
    if let Some(t) = session.default_max_turns {
        c.default_max_turns = t;
    }
    if let Some(ref cm) = session.compaction {
        c.compaction = Some(cm.clone());
    }
    if let Some(ref g) = session.enabled_tool_groups {
        c.enabled_tool_groups = g.clone();
    }
    if let Some(ref u) = session.ollama_base_url {
        c.ollama_base_url = u.clone();
    }
    if let Some(ref ts) = session.tool_summarization {
        c.tool_summarization = ts.clone();
    }
    c
}

// ── compaction prompts ──────────────────────────────────────────────
//
// The compaction system prompt is derived from goose's compaction.md
// (<https://github.com/block/goose/blob/main/crates/goose/src/prompts/compaction.md>,
// BSD-3-Clause).  Key differences from goose:
//   - The conversation text is spliced into `{{ messages }}` by the caller
//     (using [`format_messages_for_compacting`]) rather than Tera templates.
//   - The `<analysis>` section is simplified for models that don't support
//     structured thinking tags natively.
//
// The continuation texts tell the agent it's working from a summary so it
// doesn't mention the compaction or treat the summary as something the user
// wrote.  See §2.8 of the redesign doc.

/// System prompt for full compaction (auto + manual).
/// `{{ messages }}` is replaced with the formatted conversation text.
static COMPACTION_SYSTEM_PROMPT: &str = include_str!("prompts/compaction.md");

/// Continuation instruction appended after a full auto-compaction so the
/// agent knows it is working from a summary.  The most-recent user message is
/// preserved uncompacted, so the agent should continue naturally.
const AUTO_COMPACT_CONTINUATION_TEXT: &str =
    "Your context was compacted.  The previous message above contains a \
     summary of the conversation so far.  Do not mention that you read a \
     summary or that conversation summarization occurred.  Just continue \
     the conversation naturally based on the summarized context.";

/// Continuation instruction for manual compaction (user selected a range).
const MANUAL_COMPACT_CONTINUATION_TEXT: &str =
    "Your context was compacted at the user's request.  The previous message \
     above contains a summary of the conversation so far.  Do not mention \
     that you read a summary or that conversation summarization occurred.  \
     Just continue the conversation naturally based on the summarized context.";

/// System prompt for tool-pair summarization (embedded at compile time).
///
/// Instructs the model to summarize a single tool call+result pair — what was
/// requested, what happened, and any key data — so the summary can stand in
/// for the original verbose call and result.  The actual pair text is passed
/// as the user message, not embedded here (unlike full compaction).  See
/// goose's `summarize_tool_call` system prompt for the original
/// (BSD-3-Clause).
static TOOL_PAIR_SYSTEM_PROMPT: &str = include_str!("prompts/tool_pair.md");

/// System prompt for **manual multi-tool compaction** — the user selected a
/// range of messages that are all tool calls and results (no user text, no
/// assistant text).  Like the tool-pair prompt, formatted tool text goes in
/// the **user message**; this is just the short instruction.
static MULTI_TOOL_COMPACT_PROMPT: &str =
    include_str!("prompts/multi_tool_compact.md");

/// Returns true when every message in `messages` contains **only** tool
/// content — no user text, no assistant text.  This detects ranges that are
/// better served by the lighter multi-tool prompt than the full compaction
/// prompt.
fn is_tool_only(messages: &[Message]) -> bool {
    for m in messages {
        match m {
            Message::User { content } => {
                if content.iter().any(|c| !matches!(c, message::UserContent::ToolResult(_))) {
                    return false;
                }
            }
            Message::Assistant { content, .. } => {
                if content
                    .iter()
                    .any(|c| !matches!(c, message::AssistantContent::ToolCall(_)))
                {
                    return false;
                }
            }
            _ => return false, // System, etc. — not tool-only.
        }
    }
    !messages.is_empty()
}

/// Resolve the compaction token threshold from the merged config.
///
/// An absolute token count (`CompactionMode::Tokens`) is used as-is.  A
/// percentage (`CompactionMode::Percent`) is applied to the model's known
/// context window (falling back to `None` — disabled — when the window is
/// unknown).  `None` (no `compaction` set in config) disables compaction.
fn resolve_compaction_threshold(config: &Config) -> Option<usize> {
    use goop_shared::CompactionMode;
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

#[cfg(test)]
mod compaction_integration_tests {
    use super::*;
    use crate::memory::replay_log;

    /// Helper: append a completed turn (UserPrompt → AssistantText → TurnEnded)
    /// to the session's log.
    async fn add_turn(session: &Session, user: &str, assistant: &str) {
        session
            .emit(SessionEvent::UserPrompt {
                content: user.into(),
                source: PromptSource::Terminal,
            })
            .await;
        session
            .emit(SessionEvent::AssistantText(assistant.into()))
            .await;
        session
            .emit(SessionEvent::TurnEnded {
                reason: TurnEndReason::Completed,
            })
            .await;
    }

    /// Collect all `SessionEvent`s from the broadcast channel, filtering out
    /// the initial `SessionInfo` that `TransactionLog::open_at` injects.
    fn collect_events(rx: &mut broadcast::Receiver<ServerMessage>) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        while let Ok(ServerMessage::Entry(entry)) = rx.try_recv() {
            if !matches!(entry.event, SessionEvent::SessionInfo { .. }) {
                events.push(entry.event);
            }
        }
        events
    }

    /// Auto-compaction: a session that exceeds its token budget triggers
    /// `maybe_compact`, which calls the mock agent's `summarize`, appends a
    /// `Compacted` event, and the next replay shows the summary instead of the
    /// covered messages.
    #[tokio::test]
    async fn auto_compact_replaces_prefix_with_summary() {
        // Threshold of 1 token means any non-empty conversation triggers
        // compaction.
        let (session, mut rx) =
            Session::for_compaction_test("auto-compact", "SUMMARY OF CHAT", 1).await;

        // Two completed turns — enough to exceed the 1-token threshold.
        add_turn(&session, "hello", "hi there").await;
        add_turn(&session, "how are you", "doing well").await;

        // Drain the broadcast queue (the turns' events).
        let _ = collect_events(&mut rx);

        // Trigger compaction (as drain_queue would before the next prompt).
        session.maybe_compact().await;

        // A Compacted event was emitted.
        let events = collect_events(&mut rx);
        assert_eq!(events.len(), 1, "expected one Compacted event");
        match &events[0] {
            SessionEvent::Compacted {
                summary,
                model,
                manual,
                covers,
            } => {
                assert!(
                    summary.starts_with("SUMMARY OF CHAT"),
                    "summary should start with the mock summary: {summary:?}"
                );
                assert!(
                    summary.contains(AUTO_COMPACT_CONTINUATION_TEXT),
                    "summary should contain continuation text"
                );
                assert_eq!(model, "mock/model");
                assert!(!*manual, "auto-compaction should not be manual");
                assert!(
                    covers.len() >= 2,
                    "covers should include all agent-visible items"
                );
            }
            other => panic!("expected Compacted, got {other:?}"),
        }

        // Replay the log: the covered messages should be replaced by the
        // summary, but the most-recent user message is preserved uncompacted
        // (goose-style).  So we expect 2 messages: [summary+continuation,
        // most-recent user message "how are you"].
        let log = session.history.lock().await;
        let messages = replay_log(log.entries(), log.active_tip());
        drop(log);

        assert_eq!(
            messages.len(),
            2,
            "replay should show summary + preserved user msg, got {messages:?}"
        );
    }

    /// Manual compaction via `compact_range`: the user selects a range of
    /// messages, the server summarizes them, and replay shows the summary
    /// replacing those messages while uncovered messages remain.
    #[tokio::test]
    async fn manual_compact_range_preserves_uncovered_messages() {
        // No auto-compaction (huge threshold).
        let (session, mut rx) =
            Session::for_compaction_test("manual-compact", "RANGE SUMMARY", usize::MAX).await;

        // Three turns.
        add_turn(&session, "q1", "a1").await;
        add_turn(&session, "q2", "a2").await;
        add_turn(&session, "q3", "a3").await;

        let _ = collect_events(&mut rx);

        // Get the agent-visible items to find the seqs of the middle turn.
        let items = session.memory.agent_visible_items().await;
        // items: [q1, a1, q2, a2, q3, a3] — seqs 1..=6 (0 is SessionInfo).
        assert_eq!(items.len(), 6);
        // Cover the middle turn: q2 (seq 3) and a2 (seq 4).
        let covers = vec![items[2].seq, items[3].seq];

        session.compact_range_impl(covers).await;

        let events = collect_events(&mut rx);
        // compact_range_impl emits: Thinking → Compacted → TurnEnded(Completed)
        assert_eq!(events.len(), 3, "expected Thinking + Compacted + TurnEnded");
        let compacted = &events[1]; // second event is Compacted
        match compacted {
            SessionEvent::Compacted {
                summary,
                manual,
                covers,
                ..
            } => {
                assert!(
                    summary.starts_with("RANGE SUMMARY"),
                    "summary should start with the mock summary: {summary:?}"
                );
                assert!(
                    summary.contains(MANUAL_COMPACT_CONTINUATION_TEXT),
                    "summary should contain manual-continuation text"
                );
                assert!(*manual, "should be manual compaction");
                assert_eq!(*covers, vec![items[2].seq, items[3].seq]);
            }
            other => panic!("expected Compacted, got {other:?}"),
        }

        // Replay: the middle turn is replaced by the summary, but turns 1
        // and 3 remain.  So we expect: q1, a1, [summary], q3, a3 = 5 messages.
        let log = session.history.lock().await;
        let messages = replay_log(log.entries(), log.active_tip());
        drop(log);

        assert_eq!(
            messages.len(),
            5,
            "replay should have 5 messages (turn1 + summary + turn3), got {messages:?}"
        );
    }

    /// Compacting an already-compacted conversation creates a rolling summary:
    /// the second compaction's `covers` includes the first summary's seq, and
    /// replay shows only the latest summary.
    #[tokio::test]
    async fn rolling_compaction_summarizes_prior_summary() {
        let (session, mut rx) = Session::for_compaction_test("rolling", "ROLLING SUMMARY", 1).await;

        add_turn(&session, "first", "answer1").await;
        add_turn(&session, "second", "answer2").await;

        let _ = collect_events(&mut rx);

        // First compaction — covers all 4 original messages.
        session.maybe_compact().await;
        let events = collect_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::Compacted { covers, .. } => {
                assert!(covers.len() >= 2, "should cover the original messages");
            }
            _ => panic!("expected Compacted"),
        }

        // After first compaction: the most-recent user message ("second") is
        // preserved uncompacted, so we see [summary+continuation, "second"].
        let items = session.memory.agent_visible_items().await;
        assert_eq!(
            items.len(),
            2,
            "summary + preserved user msg should be visible"
        );
        let summary_seq = items[0].seq;

        // Add another turn so there are ≥ 2 items to trigger a second
        // compaction (compaction_covers requires items.len() >= 2).
        add_turn(&session, "third", "answer3").await;
        let _ = collect_events(&mut rx);

        // Second compaction — covers the first summary + the new turn.
        session.maybe_compact().await;
        let events = collect_events(&mut rx);
        assert_eq!(events.len(), 1, "expected a second Compacted event");
        match &events[0] {
            SessionEvent::Compacted {
                summary,
                covers,
                manual,
                ..
            } => {
                assert!(
                    summary.starts_with("ROLLING SUMMARY"),
                    "summary should start with the mock summary: {summary:?}"
                );
                assert!(!*manual);
                // The second compaction covers the first summary's seq.
                assert!(
                    covers.contains(&summary_seq),
                    "second compaction should cover the first summary's seq {summary_seq}, got {covers:?}"
                );
            }
            other => panic!("expected Compacted, got {other:?}"),
        }

        // Replay: the latest rolling summary + the preserved most-recent user
        // message ("third") survive.  So 2 messages.
        let log = session.history.lock().await;
        let messages = replay_log(log.entries(), log.active_tip());
        drop(log);
        assert_eq!(
            messages.len(),
            2,
            "rolling summary + preserved user msg, got {messages:?}"
        );
    }

    /// When the conversation is under the threshold, `maybe_compact` is a
    /// no-op — no `Compacted` event is emitted.
    #[tokio::test]
    async fn no_compaction_when_under_threshold() {
        let (session, mut rx) =
            Session::for_compaction_test("no-compact", "SHOULD NOT APPEAR", usize::MAX).await;

        add_turn(&session, "hello", "hi").await;
        let _ = collect_events(&mut rx);

        session.maybe_compact().await;

        let events = collect_events(&mut rx);
        assert!(events.is_empty(), "no Compacted event when under threshold");
    }
}
