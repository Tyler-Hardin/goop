//! The append-only transaction log — the single source of truth for a
//! session's conversation.
//!
//! [`TransactionLog`] bundles the entry vector, the sequence counter, and the
//! on-disk path under one struct with **private fields**.  The *only* way to
//! add an entry is [`append`](TransactionLog::append), which assigns `seq`,
//! computes `parent` from the current last entry, and stamps `ts` — all in one
//! call.  This makes the ordering invariant structural rather than
//! conventional:
//!
//! > seq order == parent-pointer order == entry-vector order
//!
//! That invariant is what the backward branch walk (for forking, §2.9 of the
//! redesign doc) depends on.  It must hold even when multiple tasks append
//! concurrently (e.g. a future background tool-pair summarizer).  By keeping
//! `next_seq` inside this struct instead of a separate `AtomicU64` on
//! `Session`, no caller can assign a seq and then lose the lock race to
//! another appender — which would produce out-of-order writes and forward
//! parent edges (`parent > seq`) that corrupt the tree.
//!
//! ## RAII
//!
//! [`open`](TransactionLog::open) handles all initialization: loading from
//! disk (with legacy migration), injecting a `SessionInfo` root if absent,
//! and persisting it for brand-new sessions.  Callers never touch `next_seq`,
//! `parent`, or the file path directly.

use std::path::{Path, PathBuf};

use chrono::Utc;
use tokio::io::AsyncWriteExt;

use crate::events::{LogEntry, SessionEvent, TurnEndReason};

// ── the log ───────────────────────────────────────────────────────

/// The append-only transaction log.
///
/// Fields are private.  See the [module docs](self) for why.
pub(crate) struct TransactionLog {
    entries: Vec<LogEntry>,
    next_seq: u64,
    /// On-disk path.  `None` for in-memory logs (tests).
    path: Option<PathBuf>,
}

impl TransactionLog {
    /// Open the transaction log for a session.
    ///
    /// The log path is derived from the session name
    /// (`~/.config/goop/sessions/<name>.jsonl`) — there is a 1:1 mapping, so
    /// the caller provides only the name.  Loads existing entries from disk
    /// (with legacy bare-event migration), injects a `SessionInfo` root if
    /// the log is empty or lacks one, and persists that root for brand-new
    /// sessions.  This is the sole production constructor — all
    /// initialization happens here (RAII).
    pub(crate) async fn open(session_name: &str) -> anyhow::Result<Self> {
        let path = crate::session::sessions_dir().join(format!("{session_name}.jsonl"));
        std::fs::create_dir_all(path.parent().expect("sessions path has a parent"))?;
        Self::open_inner(path, session_name).await
    }

    /// Test-only constructor that opens a log at an explicit path (for
    /// temp-directory control).  Production code uses [`open`](Self::open),
    /// which derives the path from the session name.
    #[cfg(test)]
    pub(crate) async fn open_at(path: PathBuf, session_name: &str) -> anyhow::Result<Self> {
        Self::open_inner(path, session_name).await
    }

    /// Shared implementation: load from disk, inject `SessionInfo`, return.
    async fn open_inner(path: PathBuf, session_name: &str) -> anyhow::Result<Self> {
        let (entries, next_seq) = load_from_file(&path)?;
        let mut log = Self {
            entries,
            next_seq,
            path: Some(path),
        };
        log.ensure_session_info(session_name).await;
        Ok(log)
    }

    /// Ensure a `SessionInfo` event is the first entry in the log.
    ///
    /// For a brand-new session (empty log) the entry is persisted; for a
    /// resumed legacy session that lacks one it lives in memory only (re-injected
    /// each load until the file is eventually rewritten).  This is metadata —
    /// skipped during agent-memory replay — so its seq need only be unique.
    async fn ensure_session_info(&mut self, session_name: &str) {
        let need_inject = self.entries.is_empty()
            || !matches!(
                self.entries.first().map(|e| &e.event),
                Some(SessionEvent::SessionInfo { .. }),
            );
        if !need_inject {
            return;
        }
        let was_empty = self.entries.is_empty();
        let seq = self.next_seq;
        self.next_seq += 1;
        let entry = LogEntry {
            seq,
            parent: None,
            ts: Utc::now(),
            event: SessionEvent::SessionInfo {
                name: session_name.to_owned(),
            },
        };
        if was_empty {
            self.persist(&entry).await;
        }
        self.entries.insert(0, entry);
    }

    /// Append an event as a new [`LogEntry`]: assign the next monotonic `seq`,
    /// compute `parent` from the current last entry, stamp `ts: now`, and push.
    /// Returns the entry so the caller can persist it (via [`persist`](Self::persist))
    /// and broadcast it.
    ///
    /// This is the **sole** mutation path.  No external code can assign seqs,
    /// push entries directly, or observe `next_seq`.
    pub(crate) fn append(&mut self, event: SessionEvent) -> LogEntry {
        let seq = self.next_seq;
        self.next_seq += 1;
        let parent = self.entries.last().map(|e| e.seq);
        let entry = LogEntry {
            seq,
            parent,
            ts: Utc::now(),
            event,
        };
        self.entries.push(entry.clone());
        entry
    }

    /// Persist a single entry to the on-disk log file (best-effort — errors
    /// are logged but not returned, matching the existing behaviour where a
    /// disk failure doesn't crash the session).
    pub(crate) async fn persist(&self, entry: &LogEntry) {
        let Some(ref path) = self.path else {
            return;
        };
        append_to_file(path, entry).await;
    }

    /// Read-only access to the full entry list.  Used by replay (agent-memory
    /// reconstruction) and history snapshots (late-joining client catch-up).
    pub(crate) fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// Clone the entry list (for [`SessionSubscriber`](crate::SessionSubscriber)
    /// history replay, which needs an owned snapshot under the lock).
    pub(crate) fn entries_vec(&self) -> Vec<LogEntry> {
        self.entries.clone()
    }
}

// ── loading & legacy migration ────────────────────────────────────

/// Load the transaction log from a JSONL file.
///
/// Each line is a [`LogEntry`] envelope.  Lines that fail to parse as a
/// `LogEntry` are retried as a bare [`SessionEvent`] (the legacy
/// pre-redesign format) and wrapped in a synthesised envelope — the `seq`
/// is the next free number, `parent` the previous entry's seq, and `ts`
/// the current time (legacy files carried no timestamps).
///
/// Returns the entries in file order plus the next free `seq`.
fn load_from_file(path: &Path) -> Result<(Vec<LogEntry>, u64), anyhow::Error> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let file = std::fs::File::open(path)?;
    let mut entries = Vec::new();
    let mut next_seq: u64 = 0;
    let mut prev_seq: Option<u64> = None;
    // Counters for synthesising tool-call IDs on legacy bare events:
    // pre-redesign `ToolCall`/`ToolResult` had no `id`, so we pair them by
    // document order (the i-th call matches the i-th result).
    let mut legacy_call_n: u64 = 0;
    let mut legacy_result_n: u64 = 0;

    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<LogEntry>(&line) {
            Ok(le) => {
                // Preserve the persisted seq; advance the counter past it.
                if le.seq >= next_seq {
                    next_seq = le.seq + 1;
                }
                le
            }
            Err(_) => match migrate_legacy_bare_event(
                &line,
                &mut next_seq,
                prev_seq,
                &mut legacy_call_n,
                &mut legacy_result_n,
            ) {
                Some(e) => e,
                None => {
                    // Truly unparseable line — skip rather than abort the
                    // whole session.  Better to lose one event than all.
                    tracing::warn!("skipping unparseable log line in {path:?}");
                    continue;
                }
            },
        };
        prev_seq = Some(entry.seq);
        entries.push(entry);
    }
    Ok((entries, next_seq))
}

/// Build a [`LogEntry`] envelope for a migrated legacy line.
fn log_envelope(seq: u64, parent: Option<u64>, event: SessionEvent) -> LogEntry {
    LogEntry {
        seq,
        parent,
        ts: Utc::now(),
        event,
    }
}

/// Migrate a legacy bare-event line (the pre-redesign on-disk format) into a
/// [`LogEntry`] envelope.
///
/// Handles three categories of legacy lines:
/// 1. **Unchanged variants** (`UserPrompt`, `Thinking`, `AssistantText`,
///    `ContextUsage`, …) — parsed directly as the current `SessionEvent`.
/// 2. **Removed turn-end variants** — mapped to `TurnEnded`:
///    `FinalResponse` → `Completed`, `Error(String)` → `Error`, and
///    `Cancelled` (unit, or `{ prompt }`) → `Cancelled { prompt }`.
///    Without this, legacy sessions would have no turn-end markers and
///    replay would commit nothing.
/// 3. **`ToolCall`/`ToolResult` without `id`** — synthesise `legacy_{n}`
///    ids, paired by document order so each call matches its result.
///
/// Returns `None` for lines that can't be interpreted at all (the caller
/// skips them).
fn migrate_legacy_bare_event(
    line: &str,
    next_seq: &mut u64,
    parent: Option<u64>,
    call_n: &mut u64,
    result_n: &mut u64,
) -> Option<LogEntry> {
    // 1. Current SessionEvent format (unchanged bare variants).
    if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
        let seq = *next_seq;
        *next_seq += 1;
        return Some(log_envelope(seq, parent, event));
    }

    // 2. Inspect as generic JSON for removed/changed variants.
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ty = v.get("type").and_then(|t| t.as_str())?;
    let data = v.get("data");
    let event = match ty {
        "FinalResponse" => SessionEvent::TurnEnded {
            reason: TurnEndReason::Completed,
        },
        "Error" => SessionEvent::TurnEnded {
            reason: TurnEndReason::Error {
                message: data
                    .and_then(|d| d.as_str())
                    .unwrap_or("(unknown error)")
                    .to_string(),
            },
        },
        "Cancelled" => SessionEvent::TurnEnded {
            reason: TurnEndReason::Cancelled {
                // Legacy unit `Cancelled` (no data) → `None` (committed).
                prompt: data
                    .and_then(|d| d.get("prompt"))
                    .and_then(|p| p.as_str())
                    .map(String::from),
            },
        },
        "ToolCall" => {
            let d = data?;
            let id = format!("legacy_{call_n}");
            *call_n += 1;
            let name = d.get("name").and_then(|n| n.as_str())?.to_string();
            let arguments = d
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            }
        }
        "ToolResult" => {
            let d = data?;
            let id = format!("legacy_{result_n}");
            *result_n += 1;
            let content = d
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            SessionEvent::ToolResult { id, content }
        }
        _ => return None,
    };
    let seq = *next_seq;
    *next_seq += 1;
    Some(log_envelope(seq, parent, event))
}

/// Append a single [`LogEntry`] as a JSON line to the events file (best-effort).
async fn append_to_file(path: &Path, entry: &LogEntry) {
    let json = match serde_json::to_string(entry) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("failed to serialize log entry: {e}");
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
        tracing::error!("failed to write entry to {path:?}: {e}");
    }
    // Sync to ensure the data is on disk before we read it back (tests
    // do this immediately after open()).
    let _ = file.sync_all().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::PromptSource;

    /// A brand-new session gets a `SessionInfo` root persisted as seq 0.
    #[tokio::test]
    async fn open_new_session_injects_session_info() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");
        let log = TransactionLog::open_at(path.clone(), "my-session")
            .await
            .unwrap();

        assert_eq!(log.entries().len(), 1);
        assert_eq!(log.entries()[0].seq, 0);
        assert_eq!(log.entries()[0].parent, None);
        assert!(matches!(
            &log.entries()[0].event,
            SessionEvent::SessionInfo { name } if name == "my-session"
        ));

        // The root was persisted to disk.
        let (entries, next_seq) = load_from_file(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(next_seq, 1);
    }

    /// A resumed session that already starts with `SessionInfo` is not
    /// re-injected.
    #[tokio::test]
    async fn open_resumed_session_keeps_existing_session_info() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");
        // Write a file that already has SessionInfo as the first entry.
        let line = serde_json::to_string(&LogEntry {
            seq: 0,
            parent: None,
            ts: Utc::now(),
            event: SessionEvent::SessionInfo { name: "s".into() },
        })
        .unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let log = TransactionLog::open_at(path, "s").await.unwrap();
        assert_eq!(log.entries().len(), 1); // no injection
    }

    /// `append` assigns monotonic seqs and chains parent pointers.
    #[tokio::test]
    async fn append_assigns_seq_and_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");
        let mut log = TransactionLog::open_at(path, "s").await.unwrap();

        let e0 = log.append(SessionEvent::Thinking);
        assert_eq!(e0.seq, 1); // seq 0 is SessionInfo
        assert_eq!(e0.parent, Some(0));

        let e1 = log.append(SessionEvent::AssistantText("hi".into()));
        assert_eq!(e1.seq, 2);
        assert_eq!(e1.parent, Some(1));

        assert_eq!(log.entries().len(), 3);
    }

    /// Legacy bare-event lines are migrated into `LogEntry` envelopes with
    /// sequential seqs, parent pointers chaining to the previous entry, and
    /// synthetic timestamps.
    #[test]
    fn load_migrates_legacy_bare_events() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let line1 = serde_json::to_string(&SessionEvent::SessionInfo { name: "s".into() }).unwrap();
        let line2 = serde_json::to_string(&SessionEvent::UserPrompt {
            content: "hi".into(),
            source: PromptSource::Terminal,
        })
        .unwrap();
        std::fs::write(tmp.path(), format!("{line1}\n{line2}\n")).unwrap();

        let (entries, next_seq) = load_from_file(tmp.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].parent, None);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[1].parent, Some(0));
        assert_eq!(next_seq, 2);
    }

    /// New-format `LogEntry` lines preserve their persisted seqs (even with
    /// gaps) so seq references in later phases (Compacted.covers, etc.)
    /// stay stable across reloads.
    #[test]
    fn load_preserves_envelope_seqs() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mk = |seq, parent, event| {
            serde_json::to_string(&LogEntry {
                seq,
                parent,
                ts: Utc::now(),
                event,
            })
            .unwrap()
        };
        let l1 = mk(0, None, SessionEvent::SessionInfo { name: "s".into() });
        let l2 = mk(
            5,
            Some(0),
            SessionEvent::UserPrompt {
                content: "hi".into(),
                source: PromptSource::Web,
            },
        );
        std::fs::write(tmp.path(), format!("{l1}\n{l2}\n")).unwrap();

        let (entries, next_seq) = load_from_file(tmp.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 5);
        assert_eq!(entries[1].parent, Some(0));
        assert_eq!(next_seq, 6);
    }

    /// A mixed file (legacy bare prefix, then new-format envelope lines)
    /// assigns sequential seqs to the legacy prefix and preserves the
    /// envelope seqs that follow — the normal transition state.
    #[tokio::test]
    async fn load_handles_mixed_legacy_and_envelope() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let bare = serde_json::to_string(&SessionEvent::SessionInfo { name: "s".into() }).unwrap();
        std::fs::write(tmp.path(), format!("{bare}\n")).unwrap();

        // Append a new-format entry; its seq continues past the legacy
        // prefix (which the loader will assign 0).
        let entry = LogEntry {
            seq: 1,
            parent: Some(0),
            ts: Utc::now(),
            event: SessionEvent::UserPrompt {
                content: "hi".into(),
                source: PromptSource::Web,
            },
        };
        append_to_file(tmp.path(), &entry).await;

        let (entries, next_seq) = load_from_file(tmp.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0); // legacy line, assigned
        assert_eq!(entries[1].seq, 1); // envelope line, preserved
        assert_eq!(next_seq, 2);
    }

    /// Pre-redesign sessions use removed variants (`FinalResponse`,
    /// `Error`, `Cancelled`) and `ToolCall`/`ToolResult` without `id`.
    /// These are migrated to the current model: turn-end variants become
    /// `TurnEnded`, and tool calls/results get order-paired synthetic ids.
    #[tokio::test]
    async fn load_migrates_removed_variants_and_unids_tool_calls() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let lines = [
            // bare UserPrompt (unchanged variant — parses directly)
            serde_json::to_string(&SessionEvent::UserPrompt {
                content: "run ls".into(),
                source: PromptSource::Web,
            })
            .unwrap(),
            // legacy ToolCall with no id
            r#"{"type":"ToolCall","data":{"name":"shell","arguments":{"command":"ls"}}}"#
                .to_string(),
            // legacy ToolResult with no id (pairs with the call above)
            r#"{"type":"ToolResult","data":{"content":"a.txt"}}"#.to_string(),
            // legacy turn-end (unit FinalResponse)
            r#"{"type":"FinalResponse"}"#.to_string(),
        ];
        std::fs::write(tmp.path(), format!("{}\n", lines.join("\n"))).unwrap();

        let (entries, next_seq) = load_from_file(tmp.path()).unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0].event, SessionEvent::UserPrompt { .. }));
        // ToolCall got a synthetic id; ToolResult got the matching one.
        let call_id = match &entries[1].event {
            SessionEvent::ToolCall { id, name, .. } => {
                assert_eq!(name, "shell");
                id.clone()
            }
            other => panic!("expected ToolCall, got {other:?}"),
        };
        match &entries[2].event {
            SessionEvent::ToolResult { id, content } => {
                assert_eq!(id, &call_id); // paired by order
                assert_eq!(content, "a.txt");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // FinalResponse → TurnEnded { Completed }
        match &entries[3].event {
            SessionEvent::TurnEnded {
                reason: TurnEndReason::Completed,
            } => {}
            other => panic!("expected TurnEnded::Completed, got {other:?}"),
        }
        assert_eq!(next_seq, 4);
    }

    /// `LogEntry` round-trips through serde, including the nested tagged
    /// `SessionEvent` payload.
    #[test]
    fn log_entry_serde_roundtrip() {
        let entry = LogEntry {
            seq: 42,
            parent: Some(41),
            ts: Utc::now(),
            event: SessionEvent::TurnEnded {
                reason: TurnEndReason::Cancelled {
                    prompt: Some("hey".into()),
                },
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.parent, Some(41));
        match back.event {
            SessionEvent::TurnEnded {
                reason: TurnEndReason::Cancelled { prompt: Some(p) },
            } => assert_eq!(p, "hey"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
