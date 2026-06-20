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

mod replay;
mod transaction_log;

pub(crate) use replay::{
    VisibleItem, collect_branch, count_tool_calls, extract_tool_pair_messages,
    last_prompt_boundary, replay_log, replay_visible, tool_call_ids,
};
pub(crate) use transaction_log::TransactionLog;

use std::path::PathBuf;
use std::sync::Arc;

use rig::completion::Message;
use rig::memory::{ConversationMemory, MemoryError};
use rig_memory::{HeuristicTokenCounter, TokenCounter};
use tokio::sync::Mutex;

use crate::config;

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
/// Holds an `Arc<Mutex<TransactionLog>>` that is **the same allocation** the
/// [`Session`](crate::Session) appends to on every `emit()`, so `load()`
/// always sees the latest log.  A cheap heuristic token counter is kept for
/// the context-usage progress bar.
#[derive(Clone)]
pub struct LogReplayMemory {
    history: Arc<Mutex<TransactionLog>>,
    counter: HeuristicTokenCounter,
}

impl LogReplayMemory {
    /// Wrap a shared log.  The caller (the session) must hold (or share) the
    /// same `Arc` so that emitted events are visible here.
    pub fn new(history: Arc<Mutex<TransactionLog>>) -> Self {
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
        let messages = replay_log(log.entries(), log.active_tip());
        messages.iter().map(|m| self.counter.count(m)).sum()
    }

    /// The agent-visible items (post-replay), each tagged with its source
    /// `seq`.  Used by compaction to decide what to summarize and to build
    /// the `Compacted.covers` list.
    pub(crate) async fn agent_visible_items(&self) -> Vec<VisibleItem> {
        let log = self.history.lock().await;
        replay_visible(log.entries(), log.active_tip())
    }

    /// Approximate token count of an arbitrary message list, using the same
    /// heuristic counter as [`estimated_tokens`](Self::estimated_tokens).
    pub(crate) fn count_tokens(&self, messages: &[Message]) -> usize {
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
            Ok(replay_log(log.entries(), log.active_tip()))
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
pub(crate) fn build_session_memory(history: Arc<Mutex<TransactionLog>>) -> SessionMemory {
    LogReplayMemory::new(history)
}

// ── context-length lookup ─────────────────────────────────────────

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
