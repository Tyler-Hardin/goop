//! Pure decision logic for conversation compaction and tool-pair summarization.
//!
//! These functions are the *testable core* of the compaction system — they
//! take the agent-visible items (a pure replay projection) and decide *what*
//! to summarize.  The actual LLM summarization call and event emission live
//! in [`Session`](crate::Session) as thin glue around these decisions.
//!
//! Extracting the decisions here lets us unit-test the trigger logic,
//! most-recent-turn protection, min-token filtering, batch truncation, and
//! revalidation — all without a mock LLM.  See `docs/compaction-redesign.md`
//! §2.6 (two-tier summarization) and §5.3 (the snapshot → summarize →
//! revalidate lifecycle).

use std::collections::HashSet;

use rig::completion::{AssistantContent, Message};

use crate::config::ToolSummarizationConfig;

use super::replay::{
    VisibleItem, count_tool_calls, extract_tool_pair_messages, last_prompt_boundary, tool_call_ids,
};

// ── defaults ──

/// Default tool-call count that triggers summarization.
pub(crate) const DEFAULT_TOOL_SUMMARY_TRIGGER: usize = 15;
/// Default minimum token count for a pair to be worth summarizing.
pub(crate) const DEFAULT_TOOL_SUMMARY_MIN_TOKENS: usize = 2000;
/// Default maximum pairs summarized per invocation (bounds latency/cost).
pub(crate) const DEFAULT_TOOL_SUMMARY_BATCH: usize = 10;

// ── full compaction (tier 2) ──

/// Decide whether the agent-visible conversation should be compacted into a
/// rolling summary.  Returns the `covers` seqs (every agent-visible item) if
/// compaction should fire, or `None` if the conversation is too small or
/// under the token threshold.
///
/// `covers` spans the entire agent-visible prefix — the in-progress prompt
/// (handled by rig) is not among them and is preserved.  See §2.6.
pub(crate) fn compaction_covers(
    items: &[VisibleItem],
    threshold: usize,
    tokens: usize,
) -> Option<Vec<u64>> {
    if items.len() < 2 || tokens < threshold {
        return None;
    }
    Some(items.iter().map(|i| i.seq).collect())
}

// ── tool-pair summarization (tier 1) ──

/// A tool-call+result pair selected for summarization.
pub(crate) struct ToolSummaryCandidate {
    pub id: String,
    pub call_msg: Message,
    pub result_msg: Message,
}

/// Select tool-call+result pairs eligible for summarization, respecting:
///
/// - **trigger threshold** — the agent-visible tool-call count must meet it
/// - **most-recent-turn protection** — the LLM may still reference
///   just-finished calls, so calls from the latest turn are excluded
/// - **min-tokens filter** — only verbose pairs (big file reads, long shell
///   output) are worth summarizing
/// - **batch-size limit** — bound latency/cost per invocation by summarizing
///   only the oldest qualifying batch
///
/// Returns candidates in chronological order (oldest first).  An empty vec
/// means "nothing to do" (disabled, below trigger, or no qualifying pairs).
/// See §5.3 step 1.
pub(crate) fn select_tool_summary_candidates(
    items: &[VisibleItem],
    config: &ToolSummarizationConfig,
    count_tokens: impl Fn(&[Message]) -> usize,
) -> Vec<ToolSummaryCandidate> {
    if !config.enabled {
        return Vec::new();
    }

    let trigger = config
        .trigger_tool_count
        .unwrap_or(DEFAULT_TOOL_SUMMARY_TRIGGER);
    let min_tokens = config.min_tokens.unwrap_or(DEFAULT_TOOL_SUMMARY_MIN_TOKENS);

    // Trigger: not enough tool calls to bother.
    if count_tool_calls(items) < trigger {
        return Vec::new();
    }

    // Protect the most-recent turn's tool calls — the LLM may still
    // reference them in a follow-up.
    let protect_from = last_prompt_boundary(items);
    let candidate_ids = tool_call_ids(items, protect_from);

    // Select pairs whose combined token count exceeds the threshold.
    let mut candidates = Vec::new();
    for id in &candidate_ids {
        if let Some((call_msg, result_msg)) = extract_tool_pair_messages(items, id) {
            let tokens = count_tokens(&[call_msg.clone(), result_msg.clone()]);
            if tokens >= min_tokens {
                candidates.push(ToolSummaryCandidate {
                    id: id.clone(),
                    call_msg,
                    result_msg,
                });
            }
        }
    }

    // Summarize the oldest batch (bounds latency/cost before next turn).
    if candidates.len() > DEFAULT_TOOL_SUMMARY_BATCH {
        candidates.truncate(DEFAULT_TOOL_SUMMARY_BATCH);
    }
    candidates
}

/// Re-validate that a tool-call id is still agent-visible before committing
/// its summary.  Defence in depth — the conversation is serial, but this
/// guards against future concurrency where a pair vanishes between the
/// snapshot and the commit (e.g. a `Compacted` covers it, or a `Deleted`
/// removes it).  See §5.3 step 3.
pub(crate) fn revalidate_tool_summaries(
    items: &[VisibleItem],
    summaries: &[(String, String)],
) -> Vec<(String, String)> {
    let visible_ids: HashSet<&str> = items
        .iter()
        .flat_map(|item| {
            let Message::Assistant { content, .. } = &item.msg else {
                return Vec::new();
            };
            content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect();

    summaries
        .iter()
        .filter(|(id, _)| visible_ids.contains(id.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::replay_visible;
    use super::*;
    use crate::events::{LogEntry, PromptSource, SessionEvent, TurnEndReason};

    /// Build a linear `LogEntry` (parent = seq - 1, root = None).
    fn entry(seq: u64, event: SessionEvent) -> LogEntry {
        LogEntry {
            seq,
            parent: if seq == 0 { None } else { Some(seq - 1) },
            ts: chrono::Utc::now(),
            event,
        }
    }

    /// Crude token counter: total `Debug`-format length.  Good enough for
    /// relative comparisons in tests (longer results → more "tokens").
    fn approx_tokens(msgs: &[Message]) -> usize {
        msgs.iter().map(|m| format!("{m:?}").len()).sum()
    }

    // ── compaction_covers ──────────────────────────────────────────

    #[test]
    fn compaction_covers_none_when_under_threshold() {
        let items = vec![
            VisibleItem {
                seq: 0,
                msg: Message::user("hello"),
            },
            VisibleItem {
                seq: 1,
                msg: Message::user("world"),
            },
        ];
        // 2 items, 100 tokens, threshold 1000 → under budget.
        assert_eq!(compaction_covers(&items, 1000, 100), None);
    }

    #[test]
    fn compaction_covers_none_when_too_few_items() {
        let items = vec![VisibleItem {
            seq: 0,
            msg: Message::user("only one"),
        }];
        // 1 item → skip even if tokens exceed threshold.
        assert_eq!(compaction_covers(&items, 1, 100), None);
    }

    #[test]
    fn compaction_covers_all_seqs_when_over_threshold() {
        // Seqs may have gaps (forks, legacy migration) — covers must list
        // the actual item seqs, not a contiguous range.
        let items = vec![
            VisibleItem {
                seq: 0,
                msg: Message::user("a"),
            },
            VisibleItem {
                seq: 3,
                msg: Message::user("b"),
            },
            VisibleItem {
                seq: 7,
                msg: Message::user("c"),
            },
        ];
        assert_eq!(compaction_covers(&items, 100, 200), Some(vec![0, 3, 7]));
    }

    // ── select_tool_summary_candidates ─────────────────────────────

    #[test]
    fn select_returns_empty_when_disabled() {
        let log: Vec<LogEntry> = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: PromptSource::Terminal,
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
                    content: "result".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("ok".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                5,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                6,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                7,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "result".into(),
                },
            ),
            entry(8, SessionEvent::AssistantText("ok".into())),
            entry(
                9,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        let config = ToolSummarizationConfig {
            enabled: false,
            trigger_tool_count: Some(1),
            min_tokens: Some(0),
            ..Default::default()
        };
        let candidates = select_tool_summary_candidates(&items, &config, approx_tokens);
        assert!(candidates.is_empty());
    }

    #[test]
    fn select_returns_empty_when_below_trigger() {
        let log: Vec<LogEntry> = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q".into(),
                    source: PromptSource::Terminal,
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
                    content: "result".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("ok".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                5,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                6,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                7,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "result".into(),
                },
            ),
            entry(8, SessionEvent::AssistantText("ok".into())),
            entry(
                9,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        // Only 2 tool calls; trigger = 10.
        let config = ToolSummarizationConfig {
            enabled: true,
            trigger_tool_count: Some(10),
            min_tokens: Some(0),
            ..Default::default()
        };
        let candidates = select_tool_summary_candidates(&items, &config, approx_tokens);
        assert!(candidates.is_empty());
    }

    #[test]
    fn select_protects_most_recent_turn() {
        // Two turns, each with one tool call.  The most-recent turn's call
        // ("b") is protected; only "a" (from the earlier turn) is a candidate.
        let log: Vec<LogEntry> = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q1".into(),
                    source: PromptSource::Terminal,
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
                    content: "result a".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("ok".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                5,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                6,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                7,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "result b".into(),
                },
            ),
            entry(8, SessionEvent::AssistantText("ok".into())),
            entry(
                9,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        let config = ToolSummarizationConfig {
            enabled: true,
            trigger_tool_count: Some(1),
            min_tokens: Some(0),
            ..Default::default()
        };
        let candidates = select_tool_summary_candidates(&items, &config, approx_tokens);
        let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        // "a" (turn 1) is a candidate; "b" (turn 2, most-recent) is protected.
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn select_filters_by_min_tokens() {
        // Turn 1 has two calls: "short" (tiny result) and "long" (big result).
        // Turn 2 has one protected call.  With a min_tokens threshold between
        // the two pairs, only "long" should be selected.
        let log: Vec<LogEntry> = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q1".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                1,
                SessionEvent::ToolCall {
                    id: "short".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                2,
                SessionEvent::ToolResult {
                    id: "short".into(),
                    content: "s".into(),
                },
            ),
            entry(
                3,
                SessionEvent::ToolCall {
                    id: "long".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                4,
                SessionEvent::ToolResult {
                    id: "long".into(),
                    content: "x".repeat(1000),
                },
            ),
            entry(5, SessionEvent::AssistantText("ok".into())),
            entry(
                6,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                7,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                8,
                SessionEvent::ToolCall {
                    id: "recent".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                9,
                SessionEvent::ToolResult {
                    id: "recent".into(),
                    content: "y".repeat(1000),
                },
            ),
            entry(10, SessionEvent::AssistantText("ok".into())),
            entry(
                11,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        // min_tokens = 500: the "short" pair (~200 debug chars) is filtered
        // out; the "long" pair (~1200) passes.  "recent" is protected.
        let config = ToolSummarizationConfig {
            enabled: true,
            trigger_tool_count: Some(1),
            min_tokens: Some(500),
            ..Default::default()
        };
        let candidates = select_tool_summary_candidates(&items, &config, approx_tokens);
        let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["long"]);
    }

    #[test]
    fn select_truncates_to_batch_size() {
        // Turn 1 has 12 qualifying calls; turn 2 has 1 protected call.
        // The batch cap (10) limits how many are returned.
        let mut log = Vec::new();
        let mut seq = 0u64;

        // Turn 1: 12 tool calls with long results.
        log.push(entry(
            seq,
            SessionEvent::UserPrompt {
                content: "q1".into(),
                source: PromptSource::Terminal,
            },
        ));
        seq += 1;
        for i in 0..12 {
            let id = format!("c{i}");
            log.push(entry(
                seq,
                SessionEvent::ToolCall {
                    id: id.clone(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ));
            seq += 1;
            log.push(entry(
                seq,
                SessionEvent::ToolResult {
                    id,
                    content: "x".repeat(500),
                },
            ));
            seq += 1;
        }
        log.push(entry(seq, SessionEvent::AssistantText("done".into())));
        seq += 1;
        log.push(entry(
            seq,
            SessionEvent::TurnEnded {
                reason: TurnEndReason::Completed,
            },
        ));
        seq += 1;

        // Turn 2: 1 protected call.
        log.push(entry(
            seq,
            SessionEvent::UserPrompt {
                content: "q2".into(),
                source: PromptSource::Terminal,
            },
        ));
        seq += 1;
        log.push(entry(
            seq,
            SessionEvent::ToolCall {
                id: "recent".into(),
                name: "shell".into(),
                arguments: serde_json::json!({}),
            },
        ));
        seq += 1;
        log.push(entry(
            seq,
            SessionEvent::ToolResult {
                id: "recent".into(),
                content: "y".repeat(500),
            },
        ));
        seq += 1;
        log.push(entry(seq, SessionEvent::AssistantText("done".into())));
        seq += 1;
        log.push(entry(
            seq,
            SessionEvent::TurnEnded {
                reason: TurnEndReason::Completed,
            },
        ));

        let items = replay_visible(&log, None);
        let config = ToolSummarizationConfig {
            enabled: true,
            trigger_tool_count: Some(1),
            min_tokens: Some(0),
            ..Default::default()
        };
        // All 12 old pairs pass (token counter returns a fixed high value);
        // batch cap limits to 10, oldest first.
        let candidates = select_tool_summary_candidates(&items, &config, |_| 9999);
        assert_eq!(candidates.len(), 10);
        assert_eq!(candidates[0].id, "c0");
        assert_eq!(candidates[9].id, "c9");
    }

    // ── revalidate_tool_summaries ──────────────────────────────────

    #[test]
    fn revalidate_keeps_visible_ids() {
        let log: Vec<LogEntry> = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q1".into(),
                    source: PromptSource::Terminal,
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
                    content: "result a".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("ok".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                5,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                6,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                7,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "result b".into(),
                },
            ),
            entry(8, SessionEvent::AssistantText("ok".into())),
            entry(
                9,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        let summaries = vec![
            ("a".to_string(), "summary a".to_string()),
            ("b".to_string(), "summary b".to_string()),
        ];
        let validated = revalidate_tool_summaries(&items, &summaries);
        assert_eq!(validated.len(), 2);
    }

    #[test]
    fn revalidate_drops_vanished_ids() {
        // "a" was compacted away between snapshot and commit; "b" is still
        // visible.  Only "b"'s summary should survive revalidation.
        let log: Vec<LogEntry> = vec![
            entry(
                0,
                SessionEvent::UserPrompt {
                    content: "q1".into(),
                    source: PromptSource::Terminal,
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
                    content: "result a".into(),
                },
            ),
            entry(3, SessionEvent::AssistantText("ok".into())),
            entry(
                4,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            entry(
                5,
                SessionEvent::UserPrompt {
                    content: "q2".into(),
                    source: PromptSource::Terminal,
                },
            ),
            entry(
                6,
                SessionEvent::ToolCall {
                    id: "b".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                },
            ),
            entry(
                7,
                SessionEvent::ToolResult {
                    id: "b".into(),
                    content: "result b".into(),
                },
            ),
            entry(8, SessionEvent::AssistantText("ok".into())),
            entry(
                9,
                SessionEvent::TurnEnded {
                    reason: TurnEndReason::Completed,
                },
            ),
            // Compaction covers turn 1's items (seqs 0–3) — "a" vanishes.
            entry(
                10,
                SessionEvent::Compacted {
                    summary: "S1".into(),
                    model: "m".into(),
                    covers: vec![0, 1, 2, 3],
                    manual: false,
                },
            ),
        ];
        let items = replay_visible(&log, None);
        let summaries = vec![
            ("a".to_string(), "summary a".to_string()),
            ("b".to_string(), "summary b".to_string()),
        ];
        let validated = revalidate_tool_summaries(&items, &summaries);
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].0, "b");
    }
}
