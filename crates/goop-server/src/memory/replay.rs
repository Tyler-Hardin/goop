//! Log replay — deriving agent-visible messages from the transaction log.
//!
//! The transaction log records *what happened*; replay derives *what the LLM
//! sees*.  See `docs/compaction-redesign.md` §2.4–2.5 for the full design.

use std::collections::HashSet;

use rig::OneOrMany;
use rig::completion::{AssistantContent, Message};
use rig::message::{
    Text as MessageText, ToolCall as RigToolCall, ToolFunction, ToolResult as RigToolResult,
    ToolResultContent, UserContent,
};

use crate::events::{LogEntry, SessionEvent, TurnEndReason};

/// One item in the agent-visible set, tagged with the `seq` of the event
/// that produced it.  The seq is used by later phases for overlay
/// (`Edited`/`Deleted`) and compaction (`Compacted.covers`) targeting.
pub(crate) struct VisibleItem {
    pub(crate) seq: u64,
    pub(crate) msg: Message,
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
pub(crate) fn replay_visible(log: &[LogEntry]) -> Vec<VisibleItem> {
    let mut visible: Vec<VisibleItem> = Vec::new();
    let mut replay = Replay::new();

    for entry in log {
        match &entry.event {
            SessionEvent::TurnEnded { reason } => {
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
            }

            // A compaction replaces a range of agent-visible items with a
            // rolling summary.  `covers` references the seqs of the
            // *current* visible items (including prior summaries), so a
            // simple `retain` is correct even for nested compactions.
            SessionEvent::Compacted {
                summary, covers, ..
            } => {
                let cover_set: HashSet<u64> = covers.iter().copied().collect();
                visible.retain(|i| !cover_set.contains(&i.seq));
                visible.push(VisibleItem {
                    seq: entry.seq,
                    msg: Message::user(summary.clone()),
                });
            }

            _ => replay.feed(entry),
        }
    }

    // NOTE: deliberately do NOT commit a trailing un-terminated turn — see
    // the doc comment above.

    drop_orphaned_tool_pairs(&mut visible);

    visible
}

/// Replay the log into the agent-visible [`Message`] list (the shape rig
/// consumes).  Thin wrapper over [`replay_visible`].
pub(crate) fn replay_log(log: &[LogEntry]) -> Vec<Message> {
    replay_visible(log)
        .into_iter()
        .map(|item| item.msg)
        .collect()
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

    /// A `Compacted` event replaces its covered agent-visible items with the
    /// rolling summary.  Subsequent turns remain visible, and the summary
    /// itself carries the compaction's seq.
    #[test]
    fn replay_compacted_replaces_covered_items() {
        // turn 0: user "q" + assistant "a"
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(1, SessionEvent::AssistantText("a".into())),
            entry(
                2,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // compaction covers the two items produced by that turn (seqs 0,1)
            entry(
                3,
                SessionEvent::Compacted {
                    summary: "SUMMARY".into(),
                    model: "m".into(),
                    covers: vec![0, 1],
                    manual: false,
                },
            ),
            // a later turn stays visible
            entry(
                4,
                SessionEvent::UserPrompt {
                    content: "next".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                5,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let items = replay_visible(&log);
        // summary (seq 3) + the later prompt (seq 4)
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].seq, 3);
        assert_eq!(items[1].seq, 4);
        assert!(matches!(items[1].msg, Message::User { .. }));
    }

    /// Metadata/control events (`ContextSnapshot`, `Thinking`) are never
    /// agent-visible — they must not leak into the replayed message list.
    #[test]
    fn replay_skips_metadata_events() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "hi".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            // metadata interspersed
            entry(
                1,
                SessionEvent::ContextSnapshot {
                    seqs: vec![],
                    model: "m".into(),
                },
            ),
            entry(2, SessionEvent::Thinking),
            entry(3, SessionEvent::AssistantText("hello".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // a trailing snapshot (e.g. emitted then turn never ran) is also ignored
            entry(
                5,
                SessionEvent::ContextSnapshot {
                    seqs: vec![0],
                    model: "m".into(),
                },
            ),
        ];
        let msgs = replay_log(&log);
        assert_eq!(msgs.len(), 2); // user prompt + assistant text only
        assert!(matches!(msgs[0], Message::User { .. }));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("hello"));
    }
}
