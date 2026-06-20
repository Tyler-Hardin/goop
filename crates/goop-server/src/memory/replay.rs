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

use crate::events::{EditContent, LogEntry, SessionEvent, TurnEndReason};

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

            // A single tool call+result pair has been summarized.  Replaces
            // the pair (targeted by `id`) with the summary.  Because replay
            // merges consecutive calls/results into single messages, this
            // requires content-granularity surgery — splicing the target
            // call/result out of their messages and inserting the summary.
            // See §5.1–5.2 of the redesign doc.
            SessionEvent::ToolSummarized { id, summary, .. } => {
                apply_tool_summary(&mut visible, id, summary.clone(), entry.seq);
            }

            // ── overlay events: edit/delete prior agent-visible content ──
            // These modify the *committed* visible set (not the buffered
            // turn), so they're top-level arms rather than going through
            // `Replay::feed`.  Overlays arrive after their target's turn has
            // been committed, so the target is always in `visible` by the
            // time the overlay is processed.  See §2.10 of the redesign doc.
            //
            // Tool calls/results are targeted by seq, but replay merges
            // consecutive calls/results into single messages (losing
            // individual seqs).  So for tool targets we look up the event's
            // `id` in the log and do content-granularity surgery by id — the
            // same technique `apply_tool_summary` uses.  Text-bearing targets
            // (UserPrompt, AssistantText, Compacted, ToolSummarized) keep
            // their seq as the VisibleItem's seq, so a whole-item replace
            // suffices.
            SessionEvent::Edited {
                target,
                replacement,
            } => {
                apply_edit(&mut visible, log, *target, replacement);
            }
            SessionEvent::Deleted { target } => {
                apply_delete(&mut visible, log, *target);
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

/// Replace a single tool call+result pair (identified by `id`) with a
/// summary message.
///
/// Replay merges consecutive tool calls into one assistant `VisibleItem` and
/// consecutive results into one user `VisibleItem`, so the target call/result
/// may be *inside* a message alongside siblings.  This function splices the
/// target out of each message (content-granularity, like
/// [`drop_orphaned_tool_pairs`]), drops now-empty messages, and inserts the
/// summary at the call's former position.
///
/// If only one half of the pair is present (the other was already compacted
/// or deleted), that half is dropped too — the orphan net would catch it
/// anyway, but doing it here keeps the intermediate state valid.  If neither
/// half is present, the event is a no-op (defence in depth).
fn apply_tool_summary(visible: &mut Vec<VisibleItem>, id: &str, summary: String, seq: u64) {
    // Check whether the id exists at all.
    let has_call = visible.iter().any(|item| {
        matches!(&item.msg, Message::Assistant { content, .. }
            if content.iter().any(|c| matches!(c, AssistantContent::ToolCall(tc) if tc.id == id)))
    });
    let has_result = visible.iter().any(|item| {
        matches!(&item.msg, Message::User { content }
            if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr) if tr.id == id)))
    });
    if !has_call && !has_result {
        return;
    }

    // The summary goes at the call's position (the earlier of the pair).
    // If only the result survives (call already removed), use its position.
    let insert_pos = visible
        .iter()
        .position(|item| {
            matches!(&item.msg, Message::Assistant { content, .. }
            if content.iter().any(|c| matches!(c, AssistantContent::ToolCall(tc) if tc.id == id)))
        })
        .or_else(|| {
            visible.iter().position(|item| {
                matches!(&item.msg, Message::User { content }
            if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr) if tr.id == id)))
            })
        });

    let mut rebuilt: Vec<VisibleItem> = Vec::with_capacity(visible.len() + 1);
    let mut inserted = false;

    for (orig_idx, item) in visible.drain(..).enumerate() {
        // Insert the summary just before the item at the target position.
        if !inserted && insert_pos.is_some_and(|p| p == orig_idx) {
            rebuilt.push(VisibleItem {
                seq,
                msg: Message::user(summary.clone()),
            });
            inserted = true;
        }

        match item.msg {
            Message::Assistant {
                id: msg_id,
                content,
            } => {
                let items: Vec<AssistantContent> = content
                    .into_iter()
                    .filter(|c| !matches!(c, AssistantContent::ToolCall(tc) if tc.id == id))
                    .collect();
                if let Ok(oom) = OneOrMany::many(items) {
                    rebuilt.push(VisibleItem {
                        seq: item.seq,
                        msg: Message::Assistant {
                            id: msg_id,
                            content: oom,
                        },
                    });
                }
                // else: every content item was the target call → dropped.
            }
            Message::User { content } => {
                let items: Vec<UserContent> = content
                    .into_iter()
                    .filter(|c| !matches!(c, UserContent::ToolResult(tr) if tr.id == id))
                    .collect();
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

    if !inserted {
        rebuilt.push(VisibleItem {
            seq,
            msg: Message::user(summary),
        });
    }

    *visible = rebuilt;
}

// ── edit/delete overlay application ──────────────────────────────────
//
// `Edited`/`Deleted` modify the committed agent-visible set.  Tool calls and
// results are targeted by seq, but replay merges consecutive calls/results
// into single messages — so individual seqs are lost inside a merged message.
// We resolve this by looking up the target event's tool-call `id` in the log
// and operating at content granularity (by id), reusing the rebuild pattern
// from `apply_tool_summary`/`drop_orphaned_tool_pairs`.  Text-bearing targets
// keep their seq as the VisibleItem's seq, so a whole-item replace works.

/// Find the event payload at `target` seq in the log.
fn log_event_at(log: &[LogEntry], target: u64) -> Option<&SessionEvent> {
    log.iter().find(|e| e.seq == target).map(|e| &e.event)
}

/// Apply a `Deleted` overlay: hide `target` from the agent-visible set.
///
/// For a `ToolCall`/`ToolResult` target, the matching content is spliced out
/// of its (possibly merged) message by id; a message left empty is dropped.
/// The server emits a `Deleted` for *both* halves of a tool pair, so each
/// half is removed independently — the [`drop_orphaned_tool_pairs`] safety net
/// catches any edge case.  For any other target (UserPrompt, AssistantText,
/// Compacted, ToolSummarized) the whole `VisibleItem` whose seq matches is
/// removed.
fn apply_delete(visible: &mut Vec<VisibleItem>, log: &[LogEntry], target: u64) {
    let Some(event) = log_event_at(log, target) else {
        return; // target not in the log — no-op (defence in depth).
    };
    match event {
        SessionEvent::ToolCall { id, .. } => remove_tool_call_by_id(visible, id),
        SessionEvent::ToolResult { id, .. } => remove_tool_result_by_id(visible, id),
        _ => visible.retain(|i| i.seq != target),
    }
}

/// Apply an `Edited` overlay: replace `target`'s content with `replacement`.
///
/// Tool-call/result edits splice the replacement into the (possibly merged)
/// message by id, preserving the id and any sibling content.  Text edits
/// replace the whole `VisibleItem` whose seq matches (UserPrompt/Compacted/
/// ToolSummarized → new user text; AssistantText → assistant text replaced,
/// tool calls kept).  A mismatched replacement type for the target is a
/// no-op (defence in depth; the server targets the right type).
fn apply_edit(
    visible: &mut [VisibleItem],
    log: &[LogEntry],
    target: u64,
    replacement: &EditContent,
) {
    // Tool targets need the id from the log (the replacement for a ToolCall
    // carries new name/args but not the id — the id is the call's identity
    // and stays the same).
    if let Some(event) = log_event_at(log, target) {
        match (event, replacement) {
            (SessionEvent::ToolCall { id, .. }, EditContent::ToolCall { name, arguments }) => {
                replace_tool_call_by_id(visible, id, name, arguments);
                return;
            }
            (SessionEvent::ToolResult { id, .. }, EditContent::ToolResult { content }) => {
                replace_tool_result_by_id(visible, id, content);
                return;
            }
            _ => {}
        }
    }

    // Text replacement — operates on the whole VisibleItem whose seq matches.
    let EditContent::Text(text) = replacement else {
        return;
    };
    let Some(item) = visible.iter_mut().find(|i| i.seq == target) else {
        return; // target not currently visible (already deleted/compacted).
    };
    let text = text.clone();
    // Take ownership of the old message so we can move its content out,
    // then assign the rebuilt message back.
    let old = std::mem::replace(&mut item.msg, Message::user(String::new()));
    item.msg = match old {
        // UserPrompt / Compacted / ToolSummarized are plain user text.
        Message::User { .. } => Message::user(text),
        Message::Assistant { content, id } => {
            // Keep tool calls; replace all text with the single new chunk.
            // (An assistant message merges text + calls; editing the text
            // updates the narration while leaving the calls intact.)
            let mut items: Vec<AssistantContent> = content
                .into_iter()
                .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                .collect();
            if !text.is_empty() {
                items.insert(0, AssistantContent::Text(MessageText::new(text)));
            }
            match OneOrMany::many(items) {
                Ok(oom) => Message::Assistant { id, content: oom },
                // All content was text (now empty) — preserve the assistant
                // role with an empty text chunk.
                Err(_) => Message::Assistant {
                    id,
                    content: OneOrMany::one(AssistantContent::Text(
                        MessageText::new(String::new()),
                    )),
                },
            }
        }
        other => other, // ToolResult-only user messages etc. — leave as-is.
    };
}

/// Remove the `ToolCall` with `id` from any assistant message.  A message
/// left with no content is dropped.  Operates at content granularity so
/// sibling calls/text in a merged message survive.
fn remove_tool_call_by_id(visible: &mut Vec<VisibleItem>, id: &str) {
    let mut rebuilt: Vec<VisibleItem> = Vec::with_capacity(visible.len());
    for item in visible.drain(..) {
        match item.msg {
            Message::Assistant {
                id: msg_id,
                content,
            } => {
                let items: Vec<AssistantContent> = content
                    .into_iter()
                    .filter(|c| !matches!(c, AssistantContent::ToolCall(tc) if tc.id == id))
                    .collect();
                if let Ok(oom) = OneOrMany::many(items) {
                    rebuilt.push(VisibleItem {
                        seq: item.seq,
                        msg: Message::Assistant {
                            id: msg_id,
                            content: oom,
                        },
                    });
                }
                // else: every content item was the target call → dropped.
            }
            other => rebuilt.push(VisibleItem {
                seq: item.seq,
                msg: other,
            }),
        }
    }
    *visible = rebuilt;
}

/// Remove the `ToolResult` with `id` from any user message.  A message left
/// with no content is dropped.  See [`remove_tool_call_by_id`].
fn remove_tool_result_by_id(visible: &mut Vec<VisibleItem>, id: &str) {
    let mut rebuilt: Vec<VisibleItem> = Vec::with_capacity(visible.len());
    for item in visible.drain(..) {
        match item.msg {
            Message::User { content } => {
                let items: Vec<UserContent> = content
                    .into_iter()
                    .filter(|c| !matches!(c, UserContent::ToolResult(tr) if tr.id == id))
                    .collect();
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

/// Replace the `ToolCall` with `id` (keeping the id) with new name/arguments.
fn replace_tool_call_by_id(
    visible: &mut [VisibleItem],
    id: &str,
    name: &str,
    arguments: &serde_json::Value,
) {
    for item in visible.iter_mut() {
        if let Message::Assistant { content, .. } = &mut item.msg {
            for c in content.iter_mut() {
                if let AssistantContent::ToolCall(tc) = c
                    && tc.id == id
                {
                    tc.function = ToolFunction::new(name.to_string(), arguments.clone());
                }
            }
        }
    }
}

/// Replace the content of the `ToolResult` with `id`.
fn replace_tool_result_by_id(visible: &mut [VisibleItem], id: &str, content: &str) {
    let new_content = OneOrMany::one(ToolResultContent::Text(MessageText::new(
        content.to_string(),
    )));
    for item in visible.iter_mut() {
        if let Message::User { content } = &mut item.msg {
            for c in content.iter_mut() {
                if let UserContent::ToolResult(tr) = c
                    && tr.id == id
                {
                    tr.content = new_content.clone();
                }
            }
        }
    }
}

/// Extract a single tool call and its matching result from the visible items
/// as standalone [`Message`]s, suitable for LLM summarization input.
///
/// Returns `None` if either half is absent (the pair is incomplete).
pub(crate) fn extract_tool_pair_messages(
    items: &[VisibleItem],
    id: &str,
) -> Option<(Message, Message)> {
    let call = items.iter().find_map(|item| {
        let Message::Assistant { content, .. } = &item.msg else {
            return None;
        };
        for c in content.iter() {
            if let AssistantContent::ToolCall(tc) = c
                && tc.id == id
            {
                let content = OneOrMany::one(AssistantContent::ToolCall(tc.clone()));
                return Some(Message::Assistant { id: None, content });
            }
        }
        None
    });

    let result = items.iter().find_map(|item| {
        let Message::User { content } = &item.msg else {
            return None;
        };
        for c in content.iter() {
            if let UserContent::ToolResult(tr) = c
                && tr.id == id
            {
                let content = OneOrMany::one(UserContent::ToolResult(tr.clone()));
                return Some(Message::User { content });
            }
        }
        None
    });

    match (call, result) {
        (Some(c), Some(r)) => Some((c, r)),
        _ => None,
    }
}

/// Collect all tool-call IDs from the visible items, optionally excluding
/// those at or after `protect_from` (used to protect the most-recent turn's
/// calls from summarization).
pub(crate) fn tool_call_ids(items: &[VisibleItem], protect_from: usize) -> Vec<String> {
    items
        .iter()
        .take(protect_from)
        .flat_map(|item| {
            let Message::Assistant { content, .. } = &item.msg else {
                return Vec::new();
            };
            content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.id.clone()),
                    _ => None,
                })
                .collect()
        })
        .collect()
}

/// Count all tool calls across the visible items.
pub(crate) fn count_tool_calls(items: &[VisibleItem]) -> usize {
    items
        .iter()
        .map(|item| {
            let Message::Assistant { content, .. } = &item.msg else {
                return 0;
            };
            content
                .iter()
                .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                .count()
        })
        .sum()
}

/// Find the index just past the most-recent user prompt (a `User` message
/// containing text content, not just tool results).  Tool calls at or after
/// this index belong to the most-recent turn and are protected from
/// summarization.  Returns `items.len()` if no prompt is found.
pub(crate) fn last_prompt_boundary(items: &[VisibleItem]) -> usize {
    items
        .iter()
        .rposition(|item| {
            matches!(&item.msg, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::Text(_))))
        })
        .map(|i| i + 1)
        .unwrap_or(items.len())
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

    // ── ToolSummarized replay tests ───────────────────────────────

    /// A `ToolSummarized` event replaces the targeted call+result pair with
    /// the summary.  Sibling messages remain visible.
    #[test]
    fn replay_tool_summarized_replaces_pair() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "read f".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "long file contents".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("done".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // tool-pair summary replaces the call+result (ids "a")
            entry(
                5,
                SessionEvent::ToolSummarized {
                    id: "a".into(),
                    summary: "read f → long file contents".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log);
        // prompt (seq 0) + summary (seq 5) + assistant text (seq 3)
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].seq, 0);
        assert_eq!(items[1].seq, 5);
        assert_eq!(items[2].seq, 3);
    }

    /// When parallel tool calls share a merged assistant message, summarizing
    /// one removes *only its own half* — siblings survive.
    #[test]
    fn replay_tool_summarized_preserves_sibling_calls() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "multi".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            // parallel calls: a, b (merged into one assistant VisibleItem)
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            // parallel results: a, b (merged into one user VisibleItem)
            entry(
                3,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "ra".into(),
                },
            ),
            entry(
                4,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "rb".into(),
                },
            ),
            entry(
                5,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // summarize only pair "a"
            entry(
                6,
                SessionEvent::ToolSummarized {
                    id: "a".into(),
                    summary: "summary a".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log);
        // prompt (0) + summary (6) + assistant[call b] (1) + user[result b] (3)
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].seq, 0); // prompt
        assert_eq!(items[1].seq, 6); // summary
        // The remaining assistant message should still have call "b".
        let call_b = match &items[2].msg {
            Message::Assistant { content, .. } => content
                .iter()
                .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                .count(),
            _ => panic!("expected assistant"),
        };
        assert_eq!(call_b, 1); // only call "b" remains
        // The remaining user message should still have result "b".
        let result_b = match &items[3].msg {
            Message::User { content } => content
                .iter()
                .filter(|c| matches!(c, UserContent::ToolResult(_)))
                .count(),
            _ => panic!("expected user"),
        };
        assert_eq!(result_b, 1);
    }

    /// A `ToolSummarized` for a pair already swept by an earlier `Compacted`
    /// is a no-op — the call/result are gone from the visible set.
    #[test]
    fn replay_tool_summarized_after_compacted_is_noop() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "data".into(),
                },
            ),
            entry(
                3,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // full compaction covers everything (seqs 0,1,2)
            entry(
                4,
                SessionEvent::Compacted {
                    summary: "FULL".into(),
                    model: "m".into(),
                    covers: vec![0, 1, 2],
                    manual: false,
                },
            ),
            // tool summary for "a" — but "a" is already gone → no-op
            entry(
                5,
                SessionEvent::ToolSummarized {
                    id: "a".into(),
                    summary: "should not appear".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log);
        // Only the compaction summary (seq 4) should remain.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 4);
    }

    /// A `ToolSummarized` for a non-existent id is a no-op (defence in depth).
    #[test]
    fn replay_tool_summarized_missing_id_is_noop() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "data".into(),
                },
            ),
            entry(
                3,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                4,
                SessionEvent::ToolSummarized {
                    id: "nonexistent".into(),
                    summary: "ghost".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log);
        // All three original items survive; the ghost summary is not added.
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].seq, 0);
        assert_eq!(items[1].seq, 1);
        assert_eq!(items[2].seq, 2);
    }

    // ── Edited / Deleted overlay tests ─────────────────────────────

    /// Extract the concatenated text of a `Message::User` (ignoring tool
    /// results), for assertions.
    fn user_text(m: &Message) -> Option<String> {
        match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        }
    }

    /// Extract the content of the first `ToolResult` in a `Message::User`.
    fn tool_result_text(m: &Message, id: &str) -> Option<String> {
        let Message::User { content } = m else {
            return None;
        };
        for c in content.iter() {
            if let UserContent::ToolResult(tr) = c
                && tr.id == id
            {
                return Some(
                    tr.content
                        .iter()
                        .filter_map(|c| match c {
                            ToolResultContent::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                );
            }
        }
        None
    }

    /// A `Deleted` overlay removes a user prompt from the agent-visible set.
    #[test]
    fn replay_deleted_removes_user_prompt() {
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
            entry(
                2,
                SessionEvent::UserPrompt {
                    content: "second".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                3,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // delete the first prompt
            entry(4, SessionEvent::Deleted { target: 0 }),
        ];
        let items = replay_visible(&log);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 2);
        assert_eq!(user_text(&items[0].msg).as_deref(), Some("second"));
    }

    /// Deleting a tool call removes both halves of the pair (the server emits
    /// a `Deleted` for the call seq and the result seq).
    #[test]
    fn replay_deleted_removes_tool_pair() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "data".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("done".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // server emits both halves
            entry(5, SessionEvent::Deleted { target: 1 }), // call
            entry(6, SessionEvent::Deleted { target: 2 }), // result
        ];
        let items = replay_visible(&log);
        // prompt (0) + assistant text (3); the call+result pair is gone.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].seq, 0);
        assert_eq!(assistant_text(&items[1].msg).as_deref(), Some("done"));
    }

    /// Deleting one call in a merged (parallel) assistant message removes only
    /// that call — siblings survive.  Only one `Deleted` is emitted; the
    /// orphan safety net drops the now-unpaired result.
    #[test]
    fn replay_deleted_one_of_merged_calls() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "multi".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                3,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "ra".into(),
                },
            ),
            entry(
                4,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "rb".into(),
                },
            ),
            entry(
                5,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // delete only call "a"; its result is dropped by the orphan net
            entry(6, SessionEvent::Deleted { target: 1 }),
        ];
        let items = replay_visible(&log);
        // prompt (0) + assistant[call b] (1) + user[result b] (3)
        assert_eq!(items.len(), 3);
        // The surviving assistant message has only call "b".
        let calls = match &items[1].msg {
            Message::Assistant { content, .. } => content
                .iter()
                .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                .count(),
            _ => panic!("expected assistant"),
        };
        assert_eq!(calls, 1);
        // The surviving user message has only result "b".
        let results = match &items[2].msg {
            Message::User { content } => content
                .iter()
                .filter(|c| matches!(c, UserContent::ToolResult(_)))
                .count(),
            _ => panic!("expected user"),
        };
        assert_eq!(results, 1);
    }

    /// An `Edited` overlay replaces a user prompt's text in the agent view.
    #[test]
    fn replay_edited_user_prompt() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "original".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                2,
                SessionEvent::Edited {
                    target: 0,
                    replacement: crate::events::EditContent::Text("rewritten".into()),
                },
            ),
        ];
        let items = replay_visible(&log);
        assert_eq!(items.len(), 1);
        assert_eq!(user_text(&items[0].msg).as_deref(), Some("rewritten"));
    }

    /// An `Edited` overlay replaces assistant text while keeping any tool
    /// calls in the same merged message.
    #[test]
    fn replay_edited_assistant_text_keeps_calls() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(1, SessionEvent::AssistantText("narration".into())),
            entry(
                2,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                3,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "data".into(),
                },
            ),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // edit the assistant text (seq 1, first chunk of the merged msg)
            entry(
                5,
                SessionEvent::Edited {
                    target: 1,
                    replacement: crate::events::EditContent::Text("new narration".into()),
                },
            ),
        ];
        let items = replay_visible(&log);
        // prompt (0) + assistant[text + call a] (1) + user[result a] (3)
        assert_eq!(items.len(), 3);
        let (text, calls) = match &items[1].msg {
            Message::Assistant { content, .. } => {
                let t: String = content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let c = content
                    .iter()
                    .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                    .count();
                (t, c)
            }
            _ => panic!("expected assistant"),
        };
        assert_eq!(text, "new narration");
        assert_eq!(calls, 1); // call "a" survived
    }

    /// An `Edited` overlay replaces a tool result's content (targeted by the
    /// result's seq, resolved to the tool-call id for content surgery).
    #[test]
    fn replay_edited_tool_result() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "a".into(),
                    content: "original data".into(),
                },
            ),
            entry(
                3,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                4,
                SessionEvent::Edited {
                    target: 2,
                    replacement: crate::events::EditContent::ToolResult {
                        content: "sanitized".into(),
                    },
                },
            ),
        ];
        let items = replay_visible(&log);
        // prompt (0) + assistant[call a] (1) + user[result a] (2)
        assert_eq!(items.len(), 3);
        assert_eq!(
            tool_result_text(&items[2].msg, "a").as_deref(),
            Some("sanitized")
        );
    }

    /// A `Deleted` for a seq not in the log is a no-op (defence in depth).
    #[test]
    fn replay_deleted_missing_target_is_noop() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(2, SessionEvent::Deleted { target: 999 }),
        ];
        let items = replay_visible(&log);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 0);
    }

    /// Editing then deleting the same target: delete wins (the edited item is
    /// removed entirely).
    #[test]
    fn replay_edit_then_delete() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "orig".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                2,
                SessionEvent::Edited {
                    target: 0,
                    replacement: crate::events::EditContent::Text("edited".into()),
                },
            ),
            entry(3, SessionEvent::Deleted { target: 0 }),
        ];
        let items = replay_visible(&log);
        assert!(items.is_empty());
    }
}
