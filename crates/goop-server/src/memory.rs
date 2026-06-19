//! Conversation memory via append-only transaction-log replay.
//!
//! The session's on-disk event log (`<name>.jsonl`) is the single source
//! of truth for the conversation.  [`LogReplayMemory`] implements
//! [`ConversationMemory`] by replaying that log into
//! `Vec<rig::completion::Message>` — the agent's view of the conversation.
//!
//! [`ConversationMemory::append`] is a **no-op**: the session already writes
//! every event to the log during streaming (via [`Session::emit`](crate::Session::emit)),
//! so the log is always complete.  A [`TurnEnded`](SessionEvent::TurnEnded)
//! event's reason controls whether the preceding turn's content is
//! agent-visible on replay (e.g. a cancel-with-no-work drops the turn).
//!
//! See `docs/compaction-redesign.md` §2.4–2.5 for the full design.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use rig::OneOrMany;
use rig::completion::{AssistantContent, Message};
use rig::memory::{ConversationMemory, MemoryError};
use rig::message::{
    Text as MessageText, ToolCall as RigToolCall, ToolFunction, ToolResult as RigToolResult,
    ToolResultContent, UserContent,
};
use rig_memory::{HeuristicTokenCounter, TokenCounter};
use tokio::sync::Mutex;

use crate::config;
use crate::events::{LogEntry, SessionEvent, TurnEndReason};

/// Path to the global prompt history file: `~/.config/goop/history.jsonl`
///
/// Every prompt from every client (terminal, web, GUI) is appended here
/// as a JSON-encoded string (one per line).  JSONL handles multi-line
/// prompts without escaping ambiguities.
pub(crate) fn prompt_history_path() -> PathBuf {
    config::config_dir().join("history.jsonl")
}

// ── the memory type ───────────────────────────────────────────────

/// Conversation memory that derives the agent-visible message list by
/// replaying the shared transaction log.
///
/// Holds an `Arc<Mutex<Vec<LogEntry>>>` that is **the same allocation** the
/// [`Session`](crate::Session) appends to on every `emit()`, so `load()`
/// always sees the latest log.  A cheap heuristic token counter is kept for
/// the context-usage progress bar.
#[derive(Clone)]
pub struct LogReplayMemory {
    history: Arc<Mutex<Vec<LogEntry>>>,
    counter: HeuristicTokenCounter,
}

impl LogReplayMemory {
    /// Wrap a shared log.  The caller (the session) must hold (or share) the
    /// same `Arc` so that emitted events are visible here.
    pub fn new(history: Arc<Mutex<Vec<LogEntry>>>) -> Self {
        Self {
            history,
            counter: HeuristicTokenCounter::default(),
        }
    }

    /// Approximate token count of the current agent-visible messages
    /// (post-replay), using the same [`HeuristicTokenCounter`] the old
    /// file-backed memory used for the progress bar.
    pub async fn estimated_tokens(&self) -> usize {
        let log = self.history.lock().await;
        let messages = replay_log(&log);
        messages.iter().map(|m| self.counter.count(m)).sum()
    }
}

impl ConversationMemory for LogReplayMemory {
    fn load<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Message>, MemoryError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let log = self.history.lock().await;
            Ok(replay_log(&log))
        })
    }

    fn append<'a>(
        &'a self,
        _conversation_id: &'a str,
        _messages: Vec<Message>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MemoryError>> + Send + 'a>>
    {
        // No-op: the session already persists every event to the log via
        // `emit()`.  rig calls this on a clean `FinalResponse`, but the
        // messages are already in the log.
        Box::pin(async { Ok(()) })
    }

    fn clear<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MemoryError>> + Send + 'a>>
    {
        // No-op: clearing the conversation log would destroy the append-only
        // history.  (Not invoked on the agent streaming path.)
        Box::pin(async { Ok(()) })
    }
}

/// The concrete memory type used by sessions.
pub(crate) type SessionMemory = LogReplayMemory;

/// Build the session memory sharing the given log.
pub(crate) fn build_session_memory(history: Arc<Mutex<Vec<LogEntry>>>) -> SessionMemory {
    LogReplayMemory::new(history)
}

// ── log replay → agent-visible messages ───────────────────────────

/// One item in the agent-visible set, tagged with the `seq` of the event
/// that produced it.  The seq is used by later phases for overlay
/// (`Edited`/`Deleted`) and compaction (`Compacted.covers`) targeting.
struct VisibleItem {
    seq: u64,
    msg: Message,
}

/// Replay the transaction log into the agent-visible message list.
///
/// **Turn buffering:** content events (`UserPrompt`, `AssistantText`,
/// `ToolCall`, `ToolResult`) are buffered into the current turn and only
/// committed to the visible set when a [`TurnEnded`](SessionEvent::TurnEnded)
/// is seen.  This gives [`TurnEndReason`] its functional role:
/// - [`Cancelled { prompt: Some(_) }`](TurnEndReason::Cancelled) drops the
///   whole buffered turn (no work was committed).
/// - every other reason commits the turn.
///
/// **Trailing turn is dropped.**  The loop does *not* flush a turn left
/// open at the end of the log (an in-progress turn with no `TurnEnded`).
/// That is exactly what [`ConversationMemory::load`] must return, because
/// rig appends the current prompt itself — including the open turn would
/// duplicate the prompt.
///
/// **Orphan safety net:** a `ToolCall` with no matching `ToolResult`
/// (e.g. an in-flight tool call at the moment a turn was cancelled with
/// work committed) is dropped by [`drop_orphaned_tool_pairs`], since some
/// provider APIs reject an unpaired call or result.
fn replay_log(log: &[LogEntry]) -> Vec<Message> {
    let mut visible: Vec<VisibleItem> = Vec::new();
    let mut replay = Replay::new();

    for entry in log {
        if let SessionEvent::TurnEnded { reason } = &entry.event {
            // Finalise any pending assistant text/calls and tool results
            // into the buffered turn before deciding its fate.
            replay.flush_assistant();
            replay.flush_results();
            match reason {
                TurnEndReason::Cancelled { prompt: Some(_) } => {
                    // No work committed — discard the whole turn.  The
                    // prompt is handed back to the terminal for editing.
                    replay.out.clear();
                }
                // Completed / StreamEnded / Cancelled { None } /
                // MaxTurnsExceeded / Error — the turn's committed work is
                // agent-visible.
                _ => visible.append(&mut replay.out),
            }
            continue;
        }
        replay.feed(entry);
    }

    // NOTE: deliberately do NOT commit a trailing un-terminated turn — see
    // the doc comment above.

    drop_orphaned_tool_pairs(&mut visible);

    visible.into_iter().map(|item| item.msg).collect()
}

/// Streams log events into `out`, accumulating consecutive assistant
/// content (text + tool calls) into a single assistant message and
/// consecutive tool results into a single user message — mirroring rig's
/// own message history so the replayed conversation is provider-valid.
struct Replay {
    /// Messages built for the current (open) turn.
    out: Vec<VisibleItem>,
    /// Accumulated assistant text chunks (consecutive → one message).
    a_text: Vec<String>,
    /// Accumulated assistant tool calls (consecutive → one message).
    a_calls: Vec<RigToolCall>,
    /// Seq of the first chunk contributing to the pending assistant message.
    a_seq: Option<u64>,
    /// Accumulated tool results (consecutive → one user message).
    u_results: Vec<(u64, RigToolResult)>,
}

impl Replay {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            a_text: Vec::new(),
            a_calls: Vec::new(),
            a_seq: None,
            u_results: Vec::new(),
        }
    }

    /// Flush the pending assistant accumulator (text + calls) into a single
    /// assistant message.  No-op when nothing is pending.
    fn flush_assistant(&mut self) {
        if self.a_text.is_empty() && self.a_calls.is_empty() {
            self.a_seq = None;
            return;
        }
        let mut items: Vec<AssistantContent> = self
            .a_text
            .drain(..)
            .map(|t| AssistantContent::Text(MessageText::new(t)))
            .collect();
        items.extend(self.a_calls.drain(..).map(AssistantContent::ToolCall));
        // Non-empty by the guard above.
        let content = OneOrMany::many(items).expect("replay: non-empty assistant content");
        self.out.push(VisibleItem {
            seq: self.a_seq.unwrap_or(0),
            msg: Message::Assistant { id: None, content },
        });
        self.a_seq = None;
    }

    /// Flush the pending tool-result accumulator into a single user message.
    fn flush_results(&mut self) {
        if self.u_results.is_empty() {
            return;
        }
        let first_seq = self.u_results[0].0;
        let items: Vec<UserContent> = self
            .u_results
            .drain(..)
            .map(|(_, tr)| UserContent::ToolResult(tr))
            .collect();
        let content = OneOrMany::many(items).expect("replay: non-empty tool results");
        self.out.push(VisibleItem {
            seq: first_seq,
            msg: Message::User { content },
        });
    }

    /// Fold one log event into the current turn.
    fn feed(&mut self, entry: &LogEntry) {
        match &entry.event {
            SessionEvent::UserPrompt { content, .. } => {
                self.out.push(VisibleItem {
                    seq: entry.seq,
                    msg: Message::user(content.clone()),
                });
            }
            SessionEvent::AssistantText(text) => {
                // Assistant content cannot share a message with prior tool
                // results — flush them first (a new assistant turn starts).
                self.flush_results();
                if self.a_seq.is_none() {
                    self.a_seq = Some(entry.seq);
                }
                self.a_text.push(text.clone());
            }
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.flush_results();
                if self.a_seq.is_none() {
                    self.a_seq = Some(entry.seq);
                }
                self.a_calls.push(RigToolCall::new(
                    id.clone(),
                    ToolFunction::new(name.clone(), arguments.clone()),
                ));
            }
            SessionEvent::ToolResult { id, content } => {
                // Tool results are user-side: the pending assistant message
                // (the call(s)) must be flushed first.
                self.flush_assistant();
                let tr = RigToolResult {
                    id: id.clone(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(MessageText::new(
                        content.clone(),
                    ))),
                };
                self.u_results.push((entry.seq, tr));
            }
            // Metadata / control events do not contribute messages.
            _ => {}
        }
    }
}

/// Defence in depth: drop a `ToolCall` whose `ToolResult` is absent (or
/// vice-versa).  This catches an in-flight tool call from a cancelled-with-
/// work turn, and would catch imperfect `Deleted` overlays in later phases.
/// Operates at content granularity so a merged assistant message that has
/// both text and an orphaned call keeps its text.
fn drop_orphaned_tool_pairs(visible: &mut Vec<VisibleItem>) {
    let mut call_ids: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();
    for item in visible.iter() {
        match &item.msg {
            Message::Assistant { content, .. } => {
                for c in content.iter() {
                    if let AssistantContent::ToolCall(tc) = c {
                        call_ids.insert(tc.id.clone());
                    }
                }
            }
            Message::User { content } => {
                for c in content.iter() {
                    if let UserContent::ToolResult(tr) = c {
                        result_ids.insert(tr.id.clone());
                    }
                }
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

    let mut rebuilt: Vec<VisibleItem> = Vec::with_capacity(visible.len());
    for item in visible.drain(..) {
        match item.msg {
            Message::Assistant { id, content } => {
                let mut items: Vec<AssistantContent> = content.into_iter().collect();
                items.retain(|c| match c {
                    AssistantContent::ToolCall(tc) => !orphan_calls.contains(tc.id.as_str()),
                    _ => true,
                });
                if let Ok(oom) = OneOrMany::many(items) {
                    rebuilt.push(VisibleItem {
                        seq: item.seq,
                        msg: Message::Assistant { id, content: oom },
                    });
                }
                // else: every content item was an orphaned tool call → drop.
            }
            Message::User { content } => {
                let mut items: Vec<UserContent> = content.into_iter().collect();
                items.retain(|c| match c {
                    UserContent::ToolResult(tr) => !orphan_results.contains(tr.id.as_str()),
                    _ => true,
                });
                if let Ok(oom) = OneOrMany::many(items) {
                    rebuilt.push(VisibleItem {
                        seq: item.seq,
                        msg: Message::User { content: oom },
                    });
                }
            }
            other => rebuilt.push(VisibleItem {
                seq: item.seq,
                msg: other,
            }),
        }
    }
    *visible = rebuilt;
}

/// Known context window lengths (tokens) for popular models.
///
/// Keyed by (provider, model_name).  Values sourced from provider docs.
/// This table is consulted when `compaction = "N%"` is set in config.
pub fn lookup_context_length(provider: crate::config::Provider, model_name: &str) -> Option<u32> {
    use crate::config::Provider;

    match provider {
        Provider::DeepSeek => match model_name {
            // Current V4 series (and backward-compatible aliases)
            "deepseek-v4-pro" | "deepseek-v4-flash" | "deepseek-chat" => Some(1_000_000),
            "deepseek-reasoner" => Some(1_000_000),
            _ => None,
        },

        Provider::OpenAI => match model_name {
            // Legacy / still widely used
            "gpt-4o" | "gpt-4o-mini" | "gpt-4-turbo" => Some(131_072),
            "gpt-4" => Some(8_192),
            "gpt-4-32k" => Some(32_768),
            "gpt-3.5-turbo" | "gpt-3.5-turbo-16k" => Some(16_384),

            // Reasoning models
            "o1" | "o1-preview" => Some(200_000),
            "o1-mini" => Some(131_072),
            "o3-mini" | "o3" => Some(200_000),

            // Newer high-context models (2025–2026)
            "gpt-4.1" | "gpt-4.1-mini" | "gpt-4.1-nano" => Some(1_047_576),
            "gpt-5.5" | "gpt-5.5-pro" | "gpt-5.4-mini" | "gpt-5.4-nano" => Some(1_000_000),

            _ => None,
        },

        Provider::OpenRouter => {
            // OpenRouter uses "provider/model" format, e.g. "openai/gpt-4o" or "anthropic/claude-sonnet-4-6".
            // Strip the prefix and delegate to the real provider's lookup.
            if let Some((prefix, inner)) = model_name.split_once('/') {
                let inner_provider = match prefix {
                    "openai" => Provider::OpenAI,
                    "anthropic" => Provider::Anthropic,
                    "deepseek" => Provider::DeepSeek,
                    "groq" => Provider::Groq,
                    _ => return None,
                };
                lookup_context_length(inner_provider, inner)
            } else {
                None
            }
        }

        Provider::Groq => match model_name {
            "llama-3.3-70b-versatile"
            | "llama-3.1-70b-versatile"
            | "llama-3.2-90b-vision-preview" => Some(131_072),
            "llama-3.1-8b-instant" => Some(131_072),
            "mixtral-8x7b-32768" => Some(32_768),
            "gemma2-9b-it" => Some(8_192),
            // Llama 4 Scout on Groq also uses 128k
            "meta-llama/llama-4-scout-17b-16e-instruct" | "llama-4-scout-17b-16e-instruct" => {
                Some(131_072)
            }
            _ => None,
        },

        Provider::Ollama => {
            // Local models — actual context depends on the Modelfile / `num_ctx` setting at runtime.
            // These are common *maximum supported* values for popular tags (many default lower, e.g. 4k–32k).
            match model_name {
                "llama3.3" | "llama3.2" | "llama3.1" | "llama3" => Some(131_072),
                "qwen2.5" | "qwen3" | "deepseek-r1" | "deepseek-v3" => Some(131_072),
                "mistral" | "mixtral" => Some(32_768),
                "gemma2" | "gemma3" => Some(8_192),
                _ => Some(128_000), // Default to 128k for Ollama.
            }
        }

        Provider::Anthropic => match model_name {
            // Claude 4.x family (latest as of mid-2026) — many now support 1M
            "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-opus-4-8" | "claude-opus-4-1" => {
                Some(1_000_000)
            }

            // Legacy / still-supported 200k models
            "claude-3-5-sonnet-latest" | "claude-3-5-sonnet-20241022" => Some(200_000),
            "claude-3-5-haiku-latest" | "claude-3-5-haiku-20241022" => Some(200_000),
            "claude-3-opus-latest" | "claude-3-opus-20240429" => Some(200_000),
            "claude-haiku-4-5" => Some(200_000),

            _ => None,
        },

        Provider::Zai => match model_name {
            // Only GLM-5.2 is supported — 1M context window.
            "glm-5.2" => Some(1_000_000),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, event: SessionEvent) -> LogEntry {
        LogEntry {
            seq,
            parent: if seq == 0 { None } else { Some(seq - 1) },
            ts: chrono::Utc::now(),
            event,
        }
    }

    /// Extract the assistant text of a `Message::Assistant`, concatenated.
    fn assistant_text(m: &Message) -> Option<String> {
        match m {
            Message::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        }
    }

    /// A normal completed turn (prompt → text) replays to one user + one
    /// assistant message, and a trailing open turn is excluded.
    #[test]
    fn replay_completed_turn() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "hi".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(1, SessionEvent::Thinking),
            entry(2, SessionEvent::AssistantText("Hello ".into())),
            entry(3, SessionEvent::AssistantText("there".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let msgs = replay_log(&log);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], Message::User { .. }));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("Hello there"));
    }

    /// A turn still in progress at the end of the log is NOT visible —
    /// rig appends the prompt itself, so replay must omit it.
    #[test]
    fn replay_drops_trailing_open_turn() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "first".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // second turn — never terminated:
            entry(
                2,
                SessionEvent::UserPrompt {
                    content: "second".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(3, SessionEvent::AssistantText("partial".into())),
        ];
        let msgs = replay_log(&log);
        // Only the first turn (1 user, 0 assistant since no text) survives.
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::User { .. }));
    }

    /// Cancel with no committed work drops the whole turn (incl. partial text).
    #[test]
    fn replay_cancel_no_work_drops_turn() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "do thing".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(1, SessionEvent::AssistantText("let me".into())),
            entry(
                2,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Cancelled {
                        prompt: Some("do thing".into()),
                    },
                },
            ),
        ];
        assert!(replay_log(&log).is_empty());
    }

    /// Cancel after work was committed keeps the committed work; the
    /// in-flight tool call (no result) is dropped by the orphan net.
    #[test]
    fn replay_cancel_with_work_keeps_committed_drops_inflight() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "do".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "done".into(),
                },
            ),
            // in-flight call, no result:
            entry(
                3,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Cancelled { prompt: None },
                },
            ),
        ];
        let msgs = replay_log(&log);
        // user prompt + (assistant call "a" + user result "a"); call "b" dropped.
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0], Message::User { .. })); // prompt
        assert!(matches!(msgs[1], Message::Assistant { .. })); // call a
        assert!(matches!(msgs[2], Message::User { .. })); // result a
    }

    /// A multi-step tool turn replays into the canonical
    /// user / assistant(call) / user(result) / assistant(text) shape.
    #[test]
    fn replay_tool_turn_pairs_calls_and_results() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "read it".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "x".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"f"}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "x".into(),
                    content: "body".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("done".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let msgs = replay_log(&log);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(msgs[0], Message::User { .. })); // prompt
        assert!(matches!(msgs[1], Message::Assistant { .. })); // call x
        assert!(matches!(msgs[2], Message::User { .. })); // result x
        assert_eq!(assistant_text(&msgs[3]).as_deref(), Some("done"));
    }
}
