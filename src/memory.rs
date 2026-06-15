use std::{
    io::{BufRead, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rig::{completion::Message, memory::ConversationMemory, memory::MemoryError};

use crate::config;

/// Path to the global prompt history file: `~/.config/goop/history.jsonl`
///
/// Every prompt from every client (terminal, web, GUI) is appended here
/// as a JSON-encoded string (one per line).  JSONL handles multi-line
/// prompts without escaping ambiguities.
pub(crate) fn prompt_history_path() -> PathBuf {
    config::config_dir().join("history.jsonl")
}

/// A file-backed [`ConversationMemory`] that persists messages as JSONL.
///
/// Each line is a `Message` serialized with `serde_json`.  The file is
/// rewritten on every `append` (kept simple — sessions are not terabytes).
#[derive(Clone)]
pub struct FileConversationMemory {
    path: PathBuf,
    /// In-memory cache so `load` is fast; kept in sync with the file.
    messages: Arc<Mutex<Vec<Message>>>,
}

impl FileConversationMemory {
    /// Create a new file-backed memory.  If `path` already exists, its
    /// messages are loaded into the cache immediately.
    pub fn new(path: PathBuf) -> Result<Self, MemoryError> {
        let messages = if path.exists() {
            load_messages_from_file(&path)?
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            messages: Arc::new(Mutex::new(messages)),
        })
    }

    /// Return a snapshot of all messages currently in the store.
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<Message> {
        self.messages.lock().unwrap().clone()
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

fn write_messages_to_file(path: &std::path::Path, messages: &[Message]) -> Result<(), MemoryError> {
    let mut file = std::fs::File::create(path).map_err(|e| MemoryError::Backend(Box::new(e)))?;
    for msg in messages {
        let json = serde_json::to_string(msg).map_err(|e| MemoryError::Backend(Box::new(e)))?;
        writeln!(file, "{json}").map_err(|e| MemoryError::Backend(Box::new(e)))?;
    }
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
            let guard = self
                .messages
                .lock()
                .map_err(|e| MemoryError::Internal(e.to_string()))?;
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
            {
                let mut guard = self
                    .messages
                    .lock()
                    .map_err(|e| MemoryError::Internal(e.to_string()))?;
                guard.extend(messages);
                write_messages_to_file(&self.path, &guard)?;
            }
            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        _conversation_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MemoryError>> + Send + 'a>>
    {
        Box::pin(async move {
            {
                let mut guard = self
                    .messages
                    .lock()
                    .map_err(|e| MemoryError::Internal(e.to_string()))?;
                guard.clear();
                write_messages_to_file(&self.path, &guard)?;
            }
            Ok(())
        })
    }
}
