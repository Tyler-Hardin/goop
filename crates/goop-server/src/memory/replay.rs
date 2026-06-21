//! Log replay — deriving agent-visible messages from the transaction log.
//!
//! The transaction log records *what happened*; replay derives *what the LLM
//! sees*.  The core projection logic lives in [`goop_shared::build_agent_view`]
//! (the single source of truth).  This module is a **thin wrapper** that:
//!
//! 1. Calls [`goop_shared::build_agent_view`] to get individual agent-visible
//!    items.
//! 2. Merges consecutive items of the same role into rig `Message`s for
//!    provider compatibility.
//!
//! **⚠️ Changes to interpretation logic (compaction, tool summarization,
//! turn buffering, edit/delete, orphan cleanup) belong in
//! [`goop_shared::build_agent_view`], not here.**
//!
//! See `docs/compaction-redesign.md` §2.4–2.5 for the full design.

use std::collections::HashSet;

use goop_shared::{AgentVisibleItem, LogEntry};
use rig::OneOrMany;
use rig::completion::{AssistantContent, Message};
use rig::message::{
    Text as MessageText, ToolCall as RigToolCall, ToolFunction, ToolResult as RigToolResult,
    ToolResultContent, UserContent,
};

/// One item in the agent-visible set, tagged with the `seq` of the event
/// that produced it.  The seq is used by later phases for overlay
/// (`Edited`/`Deleted`) and compaction (`Compacted.covers`) targeting.
///
/// Produced by [`replay_visible`] by calling the shared
/// [`goop_shared::build_agent_view`] and merging consecutive items.
pub(crate) struct VisibleItem {
    pub(crate) seq: u64,
    pub(crate) msg: Message,
}

/// Walk the conversation tree backward from `active_tip` to the root,
/// returning the active branch in chronological (root→tip) order.
///
/// Delegates to [`goop_shared::collect_branch`] — this re-export is kept
/// for local callers (tests, session).
pub(crate) fn collect_branch(active_tip: Option<u64>, log: &[LogEntry]) -> Vec<LogEntry> {
    goop_shared::collect_branch(log, active_tip)
}

// ── the thin wrappers ───────────────────────────────────────────────

/// Replay the transaction log into the agent-visible message list.
///
/// **Branch-aware:** only the active branch (walked from `active_tip`) is
/// replayed — sibling branches (old forks) are excluded.  `active_tip = None`
/// means "last entry" (linear).
///
/// Thin wrapper: calls [`goop_shared::build_agent_view`], then merges
/// consecutive items of the same role into rig `Message`s.
///
/// ⚠️ **Do not add interpretation logic here.**  Changes to how the agent's
/// view is derived from the log belong in [`goop_shared::build_agent_view`].
pub(crate) fn replay_visible(log: &[LogEntry], active_tip: Option<u64>) -> Vec<VisibleItem> {
    let items = goop_shared::build_agent_view(log, active_tip);
    merge_consecutive(items)
}

/// Replay the log into the agent-visible [`Message`] list (the shape rig
/// consumes).  Thin wrapper over [`replay_visible`].
pub(crate) fn replay_log(log: &[LogEntry], active_tip: Option<u64>) -> Vec<Message> {
    replay_visible(log, active_tip)
        .into_iter()
        .map(|item| item.msg)
        .collect()
}

// ── merging consecutive items ───────────────────────────────────────
//
// The shared `build_agent_view` produces individual items (e.g. each
// `AssistantText` chunk is a separate item).  This merger combines
// consecutive assistant items (text + tool calls) into single `Message`
// values that rig providers accept.
//
// Merger rules:
//   UserText    → flush pending, push as standalone Message::User
//   Summary     → flush pending, push as standalone Message::User
//   AssistantText → accumulate into pending assistant
//   ToolCall    → accumulate into pending assistant
//   ToolResult  → flush pending assistant, accumulate into pending results

fn merge_consecutive(items: Vec<AgentVisibleItem>) -> Vec<VisibleItem> {
    let mut out: Vec<VisibleItem> = Vec::with_capacity(items.len());
    let mut a_text: Vec<String> = Vec::new();
    let mut a_calls: Vec<RigToolCall> = Vec::new();
    let mut a_seq: Option<u64> = None;
    let mut u_results: Vec<(u64, RigToolResult)> = Vec::new();

    for item in items {
        match item {
            AgentVisibleItem::UserText { seq, content } => {
                flush_results(&mut out, &mut u_results);
                flush_assistant(&mut out, &mut a_text, &mut a_calls, &mut a_seq);
                out.push(VisibleItem {
                    seq,
                    msg: Message::user(content),
                });
            }
            AgentVisibleItem::Summary { seq, content } => {
                flush_results(&mut out, &mut u_results);
                flush_assistant(&mut out, &mut a_text, &mut a_calls, &mut a_seq);
                out.push(VisibleItem {
                    seq,
                    msg: Message::user(content),
                });
            }
            AgentVisibleItem::AssistantText { seq, content } => {
                flush_results(&mut out, &mut u_results);
                if a_seq.is_none() {
                    a_seq = Some(seq);
                }
                a_text.push(content);
            }
            AgentVisibleItem::ToolCall {
                seq,
                id,
                name,
                arguments,
            } => {
                flush_results(&mut out, &mut u_results);
                if a_seq.is_none() {
                    a_seq = Some(seq);
                }
                a_calls.push(RigToolCall::new(
                    id,
                    ToolFunction::new(name, arguments),
                ));
            }
            AgentVisibleItem::ToolResult { seq, id, content } => {
                flush_assistant(&mut out, &mut a_text, &mut a_calls, &mut a_seq);
                let tr = RigToolResult {
                    id,
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(MessageText::new(content))),
                };
                u_results.push((seq, tr));
            }
        }
    }

    flush_results(&mut out, &mut u_results);
    flush_assistant(&mut out, &mut a_text, &mut a_calls, &mut a_seq);
    drop_orphaned_tool_pairs(&mut out);

    out
}

fn flush_assistant(
    out: &mut Vec<VisibleItem>,
    a_text: &mut Vec<String>,
    a_calls: &mut Vec<RigToolCall>,
    a_seq: &mut Option<u64>,
) {
    if a_text.is_empty() && a_calls.is_empty() {
        *a_seq = None;
        return;
    }
    let mut items: Vec<AssistantContent> = a_text
        .drain(..)
        .map(|t| AssistantContent::Text(MessageText::new(t)))
        .collect();
    items.extend(a_calls.drain(..).map(AssistantContent::ToolCall));
    let content = OneOrMany::many(items).expect("merge: non-empty assistant content");
    out.push(VisibleItem {
        seq: a_seq.unwrap_or(0),
        msg: Message::Assistant { id: None, content },
    });
    *a_seq = None;
}

fn flush_results(out: &mut Vec<VisibleItem>, u_results: &mut Vec<(u64, RigToolResult)>) {
    if u_results.is_empty() {
        return;
    }
    let first_seq = u_results[0].0;
    let items: Vec<UserContent> = u_results
        .drain(..)
        .map(|(_, tr)| UserContent::ToolResult(tr))
        .collect();
    let content = OneOrMany::many(items).expect("merge: non-empty tool results");
    out.push(VisibleItem {
        seq: first_seq,
        msg: Message::User { content },
    });
}

// ── helpers used by compaction.rs ───────────────────────────────────
//
// These operate on the *merged* `VisibleItem` list.  Since tool calls and
// results may be merged into single messages, these functions do
// content-granularity inspection — looking inside `Message::Assistant` and
// `Message::User` for individual tool calls and results.

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

// ── orphan cleanup (post-merge) ─────────────────────────────────────

/// Defence in depth: drop a `ToolCall` whose `ToolResult` is absent (or
/// vice-versa).  Operates at content granularity so a merged assistant
/// message that has both text and an orphaned call keeps its text.
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
    use crate::events::SessionEvent;
    use crate::events::TurnEndReason;

    fn entry(seq: u64, event: SessionEvent) -> LogEntry {
        LogEntry {
            seq,
            parent: if seq == 0 { None } else { Some(seq - 1) },
            ts: chrono::Utc::now(),
            event,
        }
    }

    /// Build an entry with an explicit parent (for forked logs).
    fn fork_entry(seq: u64, parent: Option<u64>, event: SessionEvent) -> LogEntry {
        LogEntry {
            seq,
            parent,
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
        let msgs = replay_log(&log, None);
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
        let msgs = replay_log(&log, None);
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
        assert!(replay_log(&log, None).is_empty());
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
        let msgs = replay_log(&log, None);
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
        let msgs = replay_log(&log, None);
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
            entry(
                3,
                SessionEvent::Compacted {
                    summary: "SUMMARY".into(),
                    model: "m".into(),
                    covers: vec![0, 1],
                    manual: false,
                },
            ),
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
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].seq, 3);
        assert_eq!(items[1].seq, 4);
        assert!(matches!(items[1].msg, Message::User { .. }));
    }

    /// Overlapping/nested compaction: a later `Compacted` whose `covers`
    /// includes the seq of a *prior* `Compacted` summary must remove that
    /// prior summary.
    #[test]
    fn replay_compacted_includes_prior_summary() {
        let log = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q1".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(1, SessionEvent::AssistantText("a1".into())),
            entry(
                2,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                3,
                SessionEvent::Compacted {
                    summary: "S1".into(),
                    model: "m".into(),
                    covers: vec![0, 1],
                    manual: false,
                },
            ),
            entry(
                4,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(5, SessionEvent::AssistantText("a2".into())),
            entry(
                6,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                7,
                SessionEvent::Compacted {
                    summary: "S2".into(),
                    model: "m".into(),
                    covers: vec![3, 4, 5],
                    manual: false,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 7);
        assert_eq!(user_text(&items[0].msg).as_deref(), Some("S2"));
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
            entry(
                5,
                SessionEvent::ContextSnapshot {
                    seqs: vec![0],
                    model: "m".into(),
                },
            ),
        ];
        let msgs = replay_log(&log, None);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], Message::User { .. }));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("hello"));
    }

    // ── ToolSummarized replay tests ───────────────────────────────

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
            entry(
                5,
                SessionEvent::ToolSummarized {
                    id: "a".into(),
                    summary: "read f → long file contents".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].seq, 0); // prompt
        assert_eq!(items[1].seq, 5); // summary
        assert_eq!(items[2].seq, 3); // assistant text
    }

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
            entry(
                6,
                SessionEvent::ToolSummarized {
                    id: "a".into(),
                    summary: "summary a".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].seq, 0); // prompt
        assert_eq!(items[1].seq, 6); // summary
        let call_b = match &items[2].msg {
            Message::Assistant { content, .. } => content
                .iter()
                .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                .count(),
            _ => panic!("expected assistant"),
        };
        assert_eq!(call_b, 1);
        let result_b = match &items[3].msg {
            Message::User { content } => content
                .iter()
                .filter(|c| matches!(c, UserContent::ToolResult(_)))
                .count(),
            _ => panic!("expected user"),
        };
        assert_eq!(result_b, 1);
    }

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
            entry(
                4,
                SessionEvent::Compacted {
                    summary: "FULL".into(),
                    model: "m".into(),
                    covers: vec![0, 1, 2],
                    manual: false,
                },
            ),
            entry(
                5,
                SessionEvent::ToolSummarized {
                    id: "a".into(),
                    summary: "should not appear".into(),
                    model: "m".into(),
                },
            ),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 4);
    }

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
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].seq, 0);
        assert_eq!(items[1].seq, 1);
        assert_eq!(items[2].seq, 2);
    }

    // ── Edited / Deleted overlay tests ─────────────────────────────

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
            entry(4, SessionEvent::Deleted { target: 0 }),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 2);
        assert_eq!(user_text(&items[0].msg).as_deref(), Some("second"));
    }

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
            entry(5, SessionEvent::Deleted { target: 1 }),
            entry(6, SessionEvent::Deleted { target: 2 }),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].seq, 0);
        assert_eq!(assistant_text(&items[1].msg).as_deref(), Some("done"));
    }

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
            entry(6, SessionEvent::Deleted { target: 1 }),
        ];
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 3);
        let calls = match &items[1].msg {
            Message::Assistant { content, .. } => content
                .iter()
                .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                .count(),
            _ => panic!("expected assistant"),
        };
        assert_eq!(calls, 1);
        let results = match &items[2].msg {
            Message::User { content } => content
                .iter()
                .filter(|c| matches!(c, UserContent::ToolResult(_)))
                .count(),
            _ => panic!("expected user"),
        };
        assert_eq!(results, 1);
    }

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
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 1);
        assert_eq!(user_text(&items[0].msg).as_deref(), Some("rewritten"));
    }

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
            entry(
                5,
                SessionEvent::Edited {
                    target: 1,
                    replacement: crate::events::EditContent::Text("new narration".into()),
                },
            ),
        ];
        let items = replay_visible(&log, None);
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
        assert_eq!(calls, 1);
    }

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
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 3);
        assert_eq!(
            tool_result_text(&items[2].msg, "a").as_deref(),
            Some("sanitized")
        );
    }

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
        let items = replay_visible(&log, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].seq, 0);
    }

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
        let items = replay_visible(&log, None);
        assert!(items.is_empty());
    }

    // ── Forking / branching tests ────────────────────────────────────

    #[test]
    fn collect_branch_linear_returns_all() {
        let log = vec![
            entry(0, SessionEvent::SessionInfo { name: "s".into() }),
            entry(
                1,
                SessionEvent::UserPrompt {
                    content: "hi".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(2, SessionEvent::Thinking),
        ];
        let branch = collect_branch(None, &log);
        assert_eq!(branch.len(), 3);
        assert_eq!(
            branch.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn collect_branch_fork_excludes_sibling() {
        let log = vec![
            fork_entry(0, None, SessionEvent::SessionInfo { name: "s".into() }),
            fork_entry(
                1,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "A".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            fork_entry(2, Some(1), SessionEvent::AssistantText("old".into())),
            fork_entry(
                3,
                Some(2),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            fork_entry(
                4,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "A'".into(),
                    source: crate::events::PromptSource::Web,
                },
            ),
            fork_entry(5, Some(4), SessionEvent::AssistantText("new".into())),
            fork_entry(
                6,
                Some(5),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let branch = collect_branch(Some(6), &log);
        assert_eq!(
            branch.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 4, 5, 6]
        );
    }

    #[test]
    fn replay_fork_shows_new_branch_only() {
        let log = vec![
            fork_entry(0, None, SessionEvent::SessionInfo { name: "s".into() }),
            fork_entry(
                1,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "old prompt".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            fork_entry(2, Some(1), SessionEvent::AssistantText("old answer".into())),
            fork_entry(
                3,
                Some(2),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            fork_entry(
                4,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "new prompt".into(),
                    source: crate::events::PromptSource::Web,
                },
            ),
            fork_entry(5, Some(4), SessionEvent::AssistantText("new answer".into())),
            fork_entry(
                6,
                Some(5),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let msgs = replay_log(&log, Some(6));
        assert_eq!(msgs.len(), 2);
        assert_eq!(user_text(&msgs[0]).as_deref(), Some("new prompt"));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("new answer"));
    }

    #[test]
    fn replay_old_branch_shows_old_branch_only() {
        let log = vec![
            fork_entry(0, None, SessionEvent::SessionInfo { name: "s".into() }),
            fork_entry(
                1,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "old prompt".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            fork_entry(2, Some(1), SessionEvent::AssistantText("old answer".into())),
            fork_entry(
                3,
                Some(2),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            fork_entry(
                4,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "new prompt".into(),
                    source: crate::events::PromptSource::Web,
                },
            ),
            fork_entry(5, Some(4), SessionEvent::AssistantText("new answer".into())),
            fork_entry(
                6,
                Some(5),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let msgs = replay_log(&log, Some(3));
        assert_eq!(msgs.len(), 2);
        assert_eq!(user_text(&msgs[0]).as_deref(), Some("old prompt"));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("old answer"));
    }

    #[test]
    fn replay_fork_preserves_shared_prefix() {
        let log = vec![
            fork_entry(0, None, SessionEvent::SessionInfo { name: "s".into() }),
            fork_entry(
                1,
                Some(0),
                SessionEvent::UserPrompt {
                    content: "p1".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            fork_entry(2, Some(1), SessionEvent::AssistantText("a1".into())),
            fork_entry(
                3,
                Some(2),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            fork_entry(
                4,
                Some(3),
                SessionEvent::UserPrompt {
                    content: "p2".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            fork_entry(5, Some(4), SessionEvent::AssistantText("a2".into())),
            fork_entry(
                6,
                Some(5),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            fork_entry(
                7,
                Some(3),
                SessionEvent::UserPrompt {
                    content: "p2'".into(),
                    source: crate::events::PromptSource::Web,
                },
            ),
            fork_entry(8, Some(7), SessionEvent::AssistantText("a2'".into())),
            fork_entry(
                9,
                Some(8),
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let msgs = replay_log(&log, Some(9));
        assert_eq!(msgs.len(), 4);
        assert_eq!(user_text(&msgs[0]).as_deref(), Some("p1"));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("a1"));
        assert_eq!(user_text(&msgs[2]).as_deref(), Some("p2'"));
        assert_eq!(assistant_text(&msgs[3]).as_deref(), Some("a2'"));
    }

    /// A `SystemPrompt` event is metadata — it must not produce an
    /// agent-visible message during replay.
    #[test]
    fn replay_skips_system_prompt() {
        let log = vec![
            entry(0, SessionEvent::SessionInfo { name: "s".into() }),
            entry(
                1,
                SessionEvent::SystemPrompt {
                    content: "You are a helpful assistant.".into(),
                },
            ),
            entry(
                2,
                SessionEvent::UserPrompt {
                    content: "hi".into(),
                    source: crate::events::PromptSource::Terminal,
                },
            ),
            entry(3, SessionEvent::AssistantText("hello".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let msgs = replay_log(&log, None);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], Message::User { .. }));
        assert_eq!(assistant_text(&msgs[1]).as_deref(), Some("hello"));
    }
}
