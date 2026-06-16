use std::{io::BufRead, path::PathBuf, sync::Arc};

use chrono::Local;
use rig::{completion::Message, memory::ConversationMemory, memory::MemoryError};
use rig_memory::{
    Compactor, HeuristicTokenCounter, MemoryPolicy, TemplateCompactor, TokenWindowMemory,
};
use tokio::sync::Mutex;

use crate::config::{self, CompactionMode, Config};

/// Path to the global prompt history file: `~/.config/goop/history.jsonl`
///
/// Every prompt from every client (terminal, web, GUI) is appended here
/// as a JSON-encoded string (one per line).  JSONL handles multi-line
/// prompts without escaping ambiguities.
pub(crate) fn prompt_history_path() -> PathBuf {
    config::config_dir().join("history.jsonl")
}

/// A file-backed [`ConversationMemory`] with optional token-budget compaction.
///
/// Compaction runs on **write** (in [`append`]), not on read, so the file on
/// disk is always `[summary, ...recent_window]`.  Restarting a session reads
/// the file as-is — no re-compaction, no context shift.
///
/// Each line is a `Message` serialized with `serde_json`.
#[derive(Clone)]
pub struct FileConversationMemory {
    path: PathBuf,
    /// In-memory cache kept in sync with the file.
    messages: Arc<Mutex<Vec<Message>>>,
    /// Token budget for compaction.  `usize::MAX` disables.
    budget: usize,
    /// Produces a rolling text summary from evicted messages.
    compactor: TemplateCompactor,
    /// Approximates token counts for the budget policy.
    counter: HeuristicTokenCounter,
}

impl FileConversationMemory {
    /// Create a new file-backed memory.
    ///
    /// If `path` already exists, its messages are loaded into the cache
    /// immediately.  Compaction parameters are resolved from `config`.
    pub fn new(path: PathBuf, config: &Config) -> Result<Self, MemoryError> {
        let messages = if path.exists() {
            load_messages_from_file(&path)?
        } else {
            Vec::new()
        };
        let budget = resolve_compaction_budget(config);
        Ok(Self {
            path,
            messages: Arc::new(Mutex::new(messages)),
            budget,
            compactor: TemplateCompactor::new()
                // Cap the rolling summary at 4 KiB so it doesn't grow unboundedly.
                .with_max_bytes(4 * 1024),
            counter: HeuristicTokenCounter::default(),
        })
    }

    /// Return a snapshot of all messages currently in the store.
    #[allow(dead_code)]
    pub async fn snapshot(&self) -> Vec<Message> {
        self.messages.lock().await.clone()
    }
}

// ── file I/O helpers ───────────────────────────────────────────────

fn load_messages_from_file(path: &std::path::Path) -> Result<Vec<Message>, MemoryError> {
    let file = std::fs::File::open(path).map_err(|e| MemoryError::Backend(Box::new(e)))?;
    let mut messages = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|e| MemoryError::Backend(Box::new(e)))?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message =
            serde_json::from_str(&line).map_err(|e| MemoryError::Backend(Box::new(e)))?;
        messages.push(msg);
    }
    Ok(messages)
}

/// Write messages to file as JSONL.  Does **not** hold the mutex —
/// callers clone the message vec first and drop their lock.
async fn write_messages_to_file(
    path: &std::path::Path,
    messages: &[Message],
) -> Result<(), MemoryError> {
    let mut content = String::new();
    for msg in messages {
        let json = serde_json::to_string(msg).map_err(|e| MemoryError::Backend(Box::new(e)))?;
        content.push_str(&json);
        content.push('\n');
    }
    tokio::fs::write(path, content)
        .await
        .map_err(|e| MemoryError::Backend(Box::new(e)))?;
    Ok(())
}

/// Copy `path` to `<stem>.<timestamp>.<ext>` before compaction overwrites it.
///
/// Produces files like `20260128_001.messages.20260128-143022.jsonl`.
async fn rotate_messages_file(path: &std::path::Path) -> Result<(), std::io::Error> {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let archive = path.with_file_name(format!("{stem}.{timestamp}{ext}"));
    tokio::fs::copy(path, &archive).await?;
    tracing::debug!("archived pre-compaction messages to {}", archive.display());
    Ok(())
}

// ── ConversationMemory impl ────────────────────────────────────────

impl ConversationMemory for FileConversationMemory {
    fn load<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Message>, MemoryError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let guard = self.messages.lock().await;
            Ok(guard.clone())
        })
    }

    fn append<'a>(
        &'a self,
        _conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MemoryError>> + Send + 'a>>
    {
        Box::pin(async move {
            let (snapshot, did_compact) = {
                let mut guard = self.messages.lock().await;
                guard.extend(messages);

                let mut did_compact = false;

                // ── compaction (on write, so the file is always compacted) ──
                if self.budget != usize::MAX {
                    let policy = TokenWindowMemory::new(self.budget, self.counter);
                    if let Ok((kept, evicted)) = policy.apply_with_demoted(guard.clone()) {
                        if !evicted.is_empty() {
                            match self.compactor.compact("default", &evicted, None).await {
                                Ok(artifact) => {
                                    let summary_msg: Message = artifact.into();
                                    let mut compacted = vec![summary_msg];
                                    compacted.extend(kept);
                                    *guard = compacted;
                                    did_compact = true;
                                }
                                Err(e) => {
                                    tracing::warn!("compaction failed, keeping full history: {e}");
                                }
                            }
                        } else {
                            *guard = kept;
                        }
                    }
                }

                (guard.clone(), did_compact)
            }; // lock dropped — I/O outside the critical section.

            // Archive the pre-compaction file before overwriting.
            if did_compact {
                let _ = rotate_messages_file(&self.path).await.inspect_err(|e| {
                    tracing::warn!("failed to archive pre-compaction messages: {e}");
                });
            }

            write_messages_to_file(&self.path, &snapshot).await
        })
    }

    fn clear<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MemoryError>> + Send + 'a>>
    {
        Box::pin(async move {
            {
                let mut guard = self.messages.lock().await;
                guard.clear();
            }
            write_messages_to_file(&self.path, &[]).await
        })
    }
}

// ── compaction budget resolution ───────────────────────────────────

/// The concrete memory type used by sessions.
pub(crate) type SessionMemory = FileConversationMemory;

/// Build the session memory.
pub(crate) fn build_session_memory(
    path: PathBuf,
    config: &Config,
) -> Result<SessionMemory, MemoryError> {
    FileConversationMemory::new(path, config)
}

/// Resolve the compaction token budget from config.
fn resolve_compaction_budget(config: &Config) -> usize {
    match &config.compaction {
        Some(CompactionMode::Tokens(n)) => *n,
        Some(CompactionMode::Percent(pct)) => {
            let pct = (*pct).min(100);
            if let Some(ctx_len) = lookup_context_length(config.provider(), config.model_name()) {
                let budget = (ctx_len as usize) * (pct as usize) / 100;
                if budget > 0 {
                    return budget;
                }
            }
            usize::MAX
        }
        None => usize::MAX,
    }
}

/// Known context window lengths (tokens) for popular models.
///
/// Keyed by (provider, model_name).  Values sourced from provider docs.
/// This table is consulted when `compaction = "N%"` is set in config.
fn lookup_context_length(provider: crate::config::Provider, model_name: &str) -> Option<u32> {
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
            "claude-3-opus-latest" | "claude-3-opus-20240229" => Some(200_000),
            "claude-haiku-4-5" => Some(200_000),

            _ => None,
        },
    }
}
