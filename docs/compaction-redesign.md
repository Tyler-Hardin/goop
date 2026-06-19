# Conversation Model Redesign — Append-Only Transaction Log

## Status

- **Goop working tree:** clean, on `master` (4083ccb)
- **Goose reference:** clean, on `main` (5dcd3ff34)
- **Backwards compat:** breaking changes OK (pre-alpha, unreleased)

---

## 1. Comparison: Goose vs Goop

### Goose's strategy — visibility-flagged recursive LLM summary

Goose stores the **full conversation** (every message ever produced, including
summarized-away originals) in a SQLite database.  Each message carries a
`MessageMetadata` struct with two independent flags:

```
user_visible:  shown in the UI
agent_visible: sent to the LLM
```

Four combinations: `default` (both), `agent_only` (summary/continuation),
`user_only` (compacted original), `invisible` (fully archived).  When the
provider is called, `fix_conversation()` filters to `agent_visible` only.

**Two-tier summarization:**

| Tier | Trigger | What happens | Scope |
|------|---------|-------------|-------|
| Tool-pair | tool-call count > cutoff (scaled to context limit) | Background task summarizes oldest batch of 10 tool call+result pairs. Originals → `agent_invisible` (stay `user_visible`). Summary → `agent_only`. | individual pairs |
| Full | token usage > 80% of context window (`GOOSE_AUTO_COMPACT_THRESHOLD`) | Synchronous LLM call summarizes ALL agent-visible messages. All originals → `agent_invisible`. Summary + continuation prompt → `agent_only`. Most-recent user message re-added as agent-visible. | entire prefix |

**Recursive:** after a full compaction the summary is `agent_visible`.  When
context fills again, the next compaction summarizes `[previous_summary, …new
messages]` → rolling summary.  Summaries of summaries.

**Progressive removal:** if the summarization LLM call itself fails with
`ContextLengthExceeded`, goose progressively strips tool-response messages from
the middle out (10 → 20 → 50 → 100%) and retries.

**Config:** `GOOSE_AUTO_COMPACT_THRESHOLD` (float, default 0.8),
`GOOSE_TOOL_PAIR_SUMMARIZATION` (bool, default true),
`GOOSE_TOOL_CALL_CUTOFF` (int, or auto-computed from context limit).

**Summarization LLM:** `complete_fast()` — tries a configured "fast model"
first, falls back to the main model.  System prompt is a detailed template
(`prompts/compaction.md`) instructing the LLM to preserve all technical
content, file names, code, errors, decisions, etc.

**Persistence:** SQLite — the full conversation with visibility flags is the
single store.  `HistoryReplaced(Conversation)` event tells the UI to swap its
entire message list.

### Goop's current strategy — destructive text-rollup eviction

Goop uses `rig_memory::CompactingMemory` with a `TokenWindowMemory` policy and
`TemplateCompactor`.  When the token budget is exceeded:

1. Older messages are **evicted** (removed) from the active window.
2. `TemplateCompactor` produces a **text rollup** (concatenation/truncation, no
   LLM call) capped at 4 KiB.
3. The file is **rewritten** as `[summary, …recent_window]`.
4. The pre-compaction file is rotated to a timestamped backup.

Two separate files:
- `<name>.jsonl` — event stream (UI history, **never compacted**, append-only)
- `<name>.messages.jsonl` — agent memory (**rewritten** on compaction)

The events file already holds the full UI history; the messages file holds the
compacted agent memory.  They're independent — the UI always shows full history
(from events), the agent sees compacted memory (from messages).

### Key differences

| Aspect | Goose | Goop (current) |
|--------|-------|----------------|
| Summarization | LLM call (semantic) | Text rollup (mechanical) |
| Originals after compaction | Preserved (agent_invisible, user_visible) | **Destroyed** (evicted from memory; rotated backup only) |
| Recursive | Yes (summary of summary) | No (flat text rollup) |
| Tool-pair summarization | Yes (fine-grained, background) | No |
| Visibility model | Flags on each message (`user_visible`/`agent_visible`) | Two separate files |
| Storage | SQLite (mutable rows) | JSONL (messages file rewritten on compaction) |
| UI after compaction | Shows full history (user_visible) + hidden summaries | Shows full history (from events file) |
| Progressive fallback | Strips tool responses middle-out | None |
| Edit/delete messages | No (can hide via flags, can't edit content) | No |
| Forking / branching | No | No |
| Per-call model identity | No (session-level config) | No (session-level config) |
| Mid-conversation model switch | Not recorded | Not recorded |
| Timestamps | Per-message (`created` field) | Implicit (file line order) |
| Config | Threshold + tool cutoff + fast model | Token budget (absolute or %) |

---

## 2. Proposed Design

### 2.1 Core principle: one append-only transaction log

The current `<name>.jsonl` events file **becomes** the single source of truth —
a detailed, append-only transaction log.  The separate `<name>.messages.jsonl`
file is **eliminated**.  Nothing is ever rewritten or deleted.

Every action is a transaction appended to the log:
- User prompts, thinking, assistant text, tool calls, tool results (existing)
- **Compaction operations** (new) — full-conversation summaries
- **Tool-pair summaries** (new) — individual tool call+result summaries
- **Context snapshots** (new) — audit record of what the LLM saw at each call
- **Edits** (new) — overlay replacing a prior event's content
- **Deletes** (new) — overlay hiding a prior event from the agent view
- **Forks** (new) — branching the conversation from a past point

Visibility is **derived from replay**, never stored as a flag.  The log records
*what happened*; consumers derive *what to show* by replaying:

- **Agent view** (what the LLM sees): replay the active branch; when a
  compaction event is encountered, replace the covered messages with the
  summary; when a tool-summary event is encountered, replace that pair with its
  summary; when an edit is encountered, use the replacement content; when a
  delete is encountered, skip the target.  The result is
  `[summaries…, uncompacted recent messages]`.

- **UI view** (what the user sees): replay all events on the active branch;
  compaction events create collapsible tree nodes; edits show edited content
  with a "show original" affordance; deletes show faded/struck-through; branch
  points show `< 1/2 >` version indicators.  Full history shown by default; a
  faint outline + `>` arrow toggles to the compacted (summary) view.

### 2.2 Log entry envelope

Instead of bare events, the log stores **entries** — an envelope carrying the
tree structure and ordering, wrapping the event payload.  One JSONL line per
entry:

```rust
/// One line in the transaction log (JSONL).
struct LogEntry {
    /// Monotonic sequence number, assigned at append time.
    seq: u64,
    /// Parent event in the conversation tree.
    /// None = root; Some(seq-1) = linear continuation; Some(other) = fork.
    parent: Option<u64>,
    /// When this entry was appended (UTC).  Enables UI features like
    /// relative timestamps ("3 min ago"), tool-call duration
    /// (ToolResult.ts - ToolCall.ts), turn duration, and idle-gap display.
    /// On the envelope, not the payload, so event variants stay clean.
    ts: chrono::DateTime<chrono::Utc>,
    /// The actual event payload.
    event: SessionEvent,
}
```

The envelope handles ordering and branching; the payload handles content.  This
separation means:
- Replay walks parent pointers (tree-aware).
- UI walks parent pointers to build the active branch.
- Compaction's `covers` references seqs on the active branch.
- Snapshots reference seqs on the active branch.

**Initial behavior:** `parent` is always `Some(seq - 1)` (linear).  The
tree-walk replay logic is implemented from the start, but forking UI lands
later — when it does, the format already supports it with no migration.

**Active tip:** the seq of the latest event on the current branch.  Tracked as
session state, persisted in `<name>.state.toml` (added when forking lands;
until then it's always the last seq).  Replay walks backward from the active
tip to the root, collecting events along the way.

### 2.2a TransactionLog — encapsulated append-only log (RAII)

The entry vector, sequence counter, and on-disk path are bundled in a
`TransactionLog` struct with **private fields**.  No external code can assign
seqs, push entries directly, or observe `next_seq`.  This makes the ordering
invariant structural rather than conventional:

> seq order == parent-pointer order == entry-vector order

That invariant is what the backward branch walk (for forking, §2.9) depends
on.  It must hold even when multiple tasks append concurrently (e.g. a future
background tool-pair summarizer).  By keeping `next_seq` inside the struct
instead of a separate `AtomicU64` on `Session`, no caller can assign a seq
and then lose the lock race to another appender — which would produce
out-of-order file writes and forward parent edges (`parent > seq`) that
corrupt the tree.

```rust
pub(crate) struct TransactionLog {
    entries: Vec<LogEntry>,   // private
    next_seq: u64,            // private
    path: Option<PathBuf>,    // private (None = in-memory, tests)
}
```

**RAII constructor** — `TransactionLog::open(path, session_name) -> Self`
handles all initialization: loading from disk (with legacy bare-event
migration), injecting a `SessionInfo` root if absent, and persisting it for
brand-new sessions.  `Session::new` just calls `open()` and broadcasts the
`SessionInfo` — it never touches seqs, parent pointers, or file paths.

**Sole mutation path** — `append(event) -> LogEntry` assigns the next
monotonic `seq`, computes `parent` from the current last entry, stamps `ts`,
and pushes.  It is sync and pure (no I/O) so it can be unit-tested without
a runtime or temp files.  Persistence is a separate `persist(&entry)` method
(best-effort async file write); the caller (`Session::emit`) orchestrates
`append → persist → broadcast` under the history lock.

**Module structure** — `src/memory/` is a module directory:
- `transaction_log.rs` — `TransactionLog` (struct, `open`, `append`,
  `persist`, `entries`), plus legacy loading/migration (private).
- `replay.rs` — pure replay functions (`replay_log`, `replay_visible`) that
  take `&[LogEntry]` and produce `Vec<Message>` or `Vec<VisibleItem>`.  This
  is a *projection* (log → agent-visible messages), not a property of the
  log — it applies compaction, overlays, and turn buffering to derive what
  the LLM sees.  Kept separate from `TransactionLog` for independent
  testability and to respect the distinction between the source of truth
  (the log) and its consumer-specific projections.
- `mod.rs` — `LogReplayMemory` (implements rig's `ConversationMemory` by
  replaying the shared `Arc<Mutex<TransactionLog>>`), context-length lookup
  table.

### 2.3 Enriched and new event types

Current events need enrichment for agent-memory reconstruction.  Critically,
`ToolCall` and `ToolResult` currently **lack the tool-call ID** needed to pair
them into rig `Message` objects:

```rust
// CURRENT (insufficient for Message reconstruction)
ToolCall { name: String, arguments: serde_json::Value }
ToolResult { content: String }

// PROPOSED (add id for pairing)
ToolCall { id: String, name: String, arguments: serde_json::Value }
ToolResult { id: String, content: String }
```

New event types:

```rust
/// A set of agent-visible events has been summarized into `summary`.
/// Replaces those events in the agent's view.  In the UI, the covered
/// events form a collapsible group.
///
/// `covers` MUST reference the seqs of the **current agent-visible items**
/// being replaced — including prior `Compacted`/`ToolSummarized` events.
/// This makes overlapping/nested compactions correct with zero special
/// cases (see §2.5 for why this matters).
Compacted {
    summary: String,
    model: String,        // which model produced the summary
    covers: Vec<u64>,     // seqs of replaced agent-visible items
    /// `manual = true` when the user explicitly selected this range.
    /// Does not change replay; used by the UI to label the group.
    manual: bool,
},

/// A single tool call+result pair has been summarized.
/// `id` matches the ToolCall/ToolResult `id` it replaces.
ToolSummarized {
    id: String,           // tool call ID
    summary: String,
    model: String,
},

/// Recorded before each LLM call.  Lists the seqs of events that are
/// agent-visible at this point (post-compaction, post-overlay), plus the
/// model that is about to see them.  The log + these seqs + the model
/// fully determine the messages the LLM received and under whose API
/// constraints they were formatted.  See §2.8.
ContextSnapshot { seqs: Vec<u64>, model: String },

/// The session's active model changed.  Appended when the user switches
/// models mid-conversation.  Does not change replay visibility — it's a
/// metadata marker.  But it lets the UI annotate "model switched from X
/// to Y here" and lets future analysis correlate quality changes with
/// model changes.  See §2.8a.
ModelChanged {
    from: String,
    to: String,
},

/// Replace the content of a prior event.  The original stays in the log;
/// replay uses the replacement for the agent view.  This is "writing into
/// the LLM's mind" — the edited content is what the LLM sees on its next
/// call.
Edited {
    target: u64,               // seq of the event being edited
    replacement: EditContent,
},

/// Hide a prior event from the agent's view.  Original preserved; replay
/// skips it.
Deleted {
    target: u64,               // seq of the event being deleted
},

/// Marks the end of a turn.  Every `run_one` invocation appends exactly one.
/// The reason determines whether the turn's content is agent-visible on
/// replay (see §2.5).  Replaces the current implicit trio of
/// `FinalResponse` / `Error` / `Cancelled` — which mushed distinct outcomes
/// together and, in one case, silently misrecorded a stream-ended-without-
/// FinalResponse as a clean completion.
TurnEnded {
    reason: TurnEndReason,
},
```

```rust
/// Why a turn ended.  Every exit path in `run_one` maps to exactly one
/// variant.  The reason is not just an audit label — it is **functionally
/// necessary for correct replay**: a `UserPrompt` whose turn ends with
/// `Cancelled { prompt: Some(_) }` is dropped from the agent-visible set
/// (the user cancelled before any work was committed).
enum TurnEndReason {
    /// Agent produced a final response naturally.
    Completed,

    /// Stream ended without a FinalResponse item (unexpected).
    /// Currently misrecorded as FinalResponse — this variant fixes that.
    StreamEnded,

    /// User cancelled the turn.
    /// `Some` → no work committed; terminal repopulates input for editing;
    ///   the entire turn (prompt + partial content) is NOT agent-visible.
    /// `None` → work committed; the turn IS agent-visible.
    Cancelled { prompt: Option<String> },

    /// Max tool-calling turns exceeded.  Committed work is agent-visible.
    MaxTurnsExceeded { max_turns: usize },

    /// A stream or tool error occurred.  Committed work may be agent-visible.
    Error { message: String },
}
```

```rust
enum EditContent {
    Text(String),                                  // UserPrompt, AssistantText
    ToolCall { name: String, arguments: Value },   // ToolCall
    ToolResult { content: String },                // ToolResult
}
```

**Full set of new `SessionEvent` variants:**

| Event | Purpose | Replaces in agent view? |
|-------|---------|------------------------|
| `Compacted { summary, model, covers, manual }` | Full-conversation summary | Yes — covered range |
| `ToolSummarized { id, summary, model }` | Individual tool-pair summary | Yes — matching pair |
| `ContextSnapshot { seqs, model }` | Audit record of agent-visible events + model at each LLM call | No (metadata only) |
| `Edited { target, replacement }` | Overlay: replace a prior event's content | Yes — target's content |
| `Deleted { target }` | Overlay: hide a prior event | Yes — target removed |
| `TurnEnded { reason }` | Explicit turn-end marker with structured reason | No (control event; but the reason **controls** whether the turn's content is visible) |
| `ModelChanged { from, to }` | Metadata: model switched mid-conversation | No (metadata only) |

All seven follow the same principle: **append-only, original preserved,
replay-derived visibility.**  No flags stored.  No rewrites.

### 2.3a Completeness principle — nothing happens without being recorded

The transaction log is a strict record: every state transition in the session
is an event.  If something happened, it's in the log.  If it's not in the log,
it didn't happen (from the conversation's perspective).

**Full audit of "things that happen":**

*Conversation events (in the log):*

| What happens | Event |
|---|---|
| Session created/resumed | `SessionInfo { name }` |
| User submits prompt | `UserPrompt { content, source }` |
| Agent starts thinking | `Thinking` |
| Agent streams text | `AssistantText(String)` |
| Agent calls tool | `ToolCall { id, name, arguments }` |
| Tool returns result | `ToolResult { id, content }` |
| **Turn ends** | **`TurnEnded { reason }`** — structured reason, every path |
| Context usage | `ContextUsage { used, limit }` |
| Full compaction | `Compacted { summary, model, covers, manual }` |
| Tool-pair summary | `ToolSummarized { id, summary, model }` |
| Context snapshot | `ContextSnapshot { seqs, model }` |
| Edit overlay | `Edited { target, replacement }` |
| Delete overlay | `Deleted { target }` |
| Model switched | `ModelChanged { from, to }` |
| Fork | `parent` pointer on `LogEntry` |
| **When it happened** | **`ts`** on every `LogEntry` (envelope, not payload) |

*Internal state (derivable from the log — not separately recorded):*

| What | Derivable how? |
|---|---|
| `is_running` | Turn running between `UserPrompt` and `TurnEnded` |
| `SessionState { running }` | Same — currently a live-only event, correctly not persisted |
| Cancel requested | Effect recorded via `TurnEnded::Cancelled`; the request is a `ClientMessage`, not a conversation event |

*Side effects (intentionally NOT recorded — delivery mechanisms, not conversation):*

| What | Why not? |
|---|---|
| Push notification sent | Delivery mechanism; the turn-end that triggered it IS recorded |
| STT transcription | Input modality; the resulting prompt IS recorded |
| WebSocket broadcast | Delivery mechanism |

The one real gap this redesign fixes is the turn-end reason.  Currently:
- `MaxTurnsError` is stringified into `Error(String)` — the structured reason
  (max turns count) is lost.
- Other stream errors are indistinguishable from MaxTurns in the log.
- A stream that ended without `FinalResponse` is **silently misrecorded as
  `FinalResponse`** — there's no way to tell a clean completion from an
  unexpected stream end.

`TurnEnded { reason }` fixes all three by giving every exit path a distinct,
structured variant.

### 2.4 Agent memory = log replay

`ConversationMemory::load()` replays the transaction log and produces
`Vec<rig::completion::Message>`:

1. Walk the active branch (parent chain from active tip to root), collecting
   entries in chronological order.
2. Build the agent-visible set, applying overlays and compaction in seq order
   (see §2.5 for the algorithm).
3. Convert the resulting agent-visible events into `rig::completion::Message`:
   - `UserPrompt` → `Message::User { text }`
   - `AssistantText` chunks → accumulate into `Message::Assistant { text }`
   - `ToolCall` → `Message::Assistant { ToolCall { id, name, args } }`
   - `ToolResult` → `Message::User { ToolResult { id, content } }`
   - `Thinking` → skip (not sent to provider)
   - `Compacted` → `Message::User { text: summary }`
   - `ToolSummarized` → `Message::User { text: summary }`
   - `ContextSnapshot` → skip (metadata, not a message)
   - `TurnEnded` → skip (control event, but its reason controls whether the
     preceding turn's content is visible — see §2.5)
   - `ModelChanged` → skip (metadata, not a message)
4. Return the resulting `Vec<Message>`.

`ConversationMemory::append()` becomes a **no-op** — the session already writes
all events to the log via `emit()` during streaming.  Rig calls `append()` on
`FinalResponse`, but the messages are already in the log.  This eliminates the
dual-write-path problem.

**Cancellation/error recovery simplifies drastically.**  The current
`preserve_committed_turns()` calls `agent.append_to_memory()` because rig only
saves to `ConversationMemory` on `FinalResponse`, so a cancelled or errored
prompt would otherwise lose the user message and every completed tool turn.
In the new model, **everything is already in the log** — `UserPrompt` is
emitted at the start of `drain_queue()`, and every `ToolCall`/`ToolResult` is
emitted during streaming.  The `TurnEnded` reason is what tells replay whether
the turn's content is agent-visible:

- `Cancelled { prompt: Some(_) }` — no work committed.  The entire turn
  (prompt + any partial content) is dropped from the agent view.  The terminal
  repopulates the input for editing.
- `Cancelled { prompt: None }` — work committed.  The turn's content IS
  agent-visible.  An in-flight tool call (emitted but no result) is handled by
  the `drop_orphaned_tool_pairs` safety net.
- `MaxTurnsExceeded` / `Error` — committed work is agent-visible.
- `Completed` / `StreamEnded` — all content is agent-visible.

No special preservation logic is needed — the log is the source of truth and
it's already complete.  The `TurnEnded` reason is the only thing replay needs
to make the right visibility decision.

### 2.5 Replay algorithm

The replay must handle overlays (edit/delete) and compaction
(tool-summarized/compacted) in a single pass.  The key invariant: every overlay
and compaction event references seqs of **current agent-visible items** —
which may themselves be prior summary events.

```rust
/// An item in the agent-visible set, tagged with its source seq.
struct VisibleItem {
    seq: u64,
    content: Message, // or a structured representation
}

fn replay(active_tip: u64, log: &[(u64, Option<u64>, SessionEvent)]) -> Vec<Message> {
    // 1. Collect the active branch (walk parents from tip to root).
    let branch: Vec<&(u64, Option<u64>, SessionEvent)> = collect_branch(active_tip, log);

    // 2. Build the agent-visible set, applying overlays and compaction.
    //    Turns are processed as units: content events are buffered into the
    //    current turn; a TurnEnded decides whether the turn is committed to
    //    the visible set or dropped (cancel-with-no-work).
    let mut visible: Vec<VisibleItem> = Vec::new();
    let mut current_turn: Vec<VisibleItem> = Vec::new(); // buffered until TurnEnded

    for (_, _, entry) in &branch {
        match &entry.event {
            // ── content events: buffer into current turn ──
            UserPrompt | AssistantText | ToolCall | ToolResult => {
                current_turn.push(VisibleItem { seq: entry.seq, content: to_message(&entry.event) });
            }

            // ── turn end: commit or drop the buffered turn ──
            TurnEnded { reason } => match reason {
                // Cancelled with no work: drop the entire turn.
                // The prompt is handed back to the terminal for editing.
                Cancelled { prompt: Some(_) } => {
                    current_turn.clear();
                }
                // All other reasons: commit the turn's content.
                _ => {
                    visible.append(&mut current_turn);
                    current_turn.clear();
                }
            },

            // ── overlays: modify the committed visible set in place ──
            // (overlays apply across turn boundaries, not within a turn)
            Edited { target, replacement } => {
                if let Some(item) = visible.iter_mut().find(|i| i.seq == *target) {
                    item.content = replacement.to_message();
                }
            }
            Deleted { target } => {
                visible.retain(|i| i.seq != *target);
            }

            // ── compaction: replace covered items with summary ──
            Compacted { summary, covers, .. } => {
                let cover_set: HashSet<u64> = covers.iter().copied().collect();
                visible.retain(|i| !cover_set.contains(&i.seq)); // remove covered (raw OR prior summary)
                visible.push(VisibleItem {
                    seq: entry.seq,
                    content: Message::user(summary),
                });
            }
            ToolSummarized { id, summary, .. } => {
                // Remove the matching tool-call + tool-result pair,
                // insert the summary in their place.
                let positions = find_tool_pair(&visible, id);
                if let Some((call_pos, result_pos)) = positions {
                    let insert_at = call_pos.min(result_pos);
                    // Remove both (highest index first to keep indices valid).
                    let (hi, lo) = (call_pos.max(result_pos), call_pos.min(result_pos));
                    visible.remove(hi);
                    visible.remove(lo);
                    visible.insert(insert_at, VisibleItem {
                        seq: entry.seq,
                        content: Message::user(summary),
                    });
                }
            }

            // ── metadata events: skip ──
            Thinking | ContextSnapshot | ModelChanged | SessionInfo | SessionState
            | ContextUsage | HistoryComplete => {}
        }
    }

    // Safety: flush any trailing turn content (shouldn't happen — every
    // run_one emits a TurnEnded — but be safe against truncated logs).
    visible.append(&mut current_turn);

    // 3. Orphan safety net: drop a ToolResult whose ToolCall was deleted
    //    (or vice versa).  Defense in depth — the UI deletes pairs, but
    //    replay tolerates imperfect deletes.  Also catches in-flight tool
    //    calls from cancelled-with-work turns (ToolCall emitted, no
    //    ToolResult before the cancel).
    drop_orphaned_tool_pairs(&mut visible);

    // 4. Convert to Vec<Message>.
    visible.into_iter().map(|i| i.content).collect()
}
```

**Why `covers` must reference current agent-visible items (including prior
summaries):**

Consider overlapping compactions:

```
seq 1–20: raw messages
seq 21: Compacted { covers: [1..20], summary: "S1" }     ← auto-compaction
seq 22–30: raw messages
seq 40: Compacted { covers: [21, 22..30], summary: "S2" } ← manual range covering S1 + new msgs
```

If `covers: [21, 22..30]` only referenced raw message events, S1 (at seq 21)
would linger in the agent view alongside S2 — corrupting the context.  By
referencing current agent-visible seqs, the `retain` call removes S1 too.
When the server generates a `Compacted` event (auto or manual), it computes
`covers` by snapshotting the current agent-visible state's seqs within the
requested range — which naturally includes prior summary events.

**Edit/delete ordering:** overlays are applied in seq order during the single
pass, so "edit then delete" and "delete then edit" both work — last action
wins for a given target.  Multiple edits to the same target: last edit wins,
all intermediates preserved in the log.

**Tool call pair handling for deletes:** deleting a `ToolCall` should also
delete its `ToolResult` (and vice versa) — otherwise the replay produces an
orphaned call or result, which some provider APIs reject.  The UI emits both
`Deleted` events explicitly; the replay's `drop_orphaned_tool_pairs` safety net
handles any edge cases (also covers compaction that covers one half of a pair
but not the other).

### 2.6 Two-tier recursive summarization (matching goose)

**Tier 1 — Tool-pair summarization (fine-grained, background, configurable)**

When the tool-call count exceeds a configurable cutoff, a background task
summarizes the oldest batch of tool call+result pairs.  Each summarized pair
gets a `ToolSummarized` event appended to the log.  The agent sees the summary;
the UI shows the original pair in a faint outline with a `>` toggle.

Configurable:
- `enabled` (bool)
- `model` — which model to use (can be a fast/cheap model, or the main model)
- `trigger` — tool-count threshold (absolute, or auto-scaled from context limit
  like goose's `compute_tool_call_cutoff`)

**Tier 2 — Full conversation compaction (coarse-grained, synchronous)**

Before each prompt, the session estimates the token count of agent-visible
messages.  If it exceeds a configurable threshold (default 80% of context
window), the session performs a synchronous LLM summarization of the entire
agent-visible prefix.  A `Compacted` event is appended to the log with
`manual = false`.  The most-recent user message is preserved (re-added as
agent-visible after the summary, like goose).

**Recursive:** after a full compaction, the summary is in the log as a regular
agent-visible item.  The next compaction's "agent-visible messages" include
that summary, so the LLM summarizes `[previous_summary, …new messages]` →
rolling summary.  Summaries of summaries.

**Progressive fallback:** if the summarization call fails with a
context-length error, progressively strip tool-result content from the middle
out (like goose) and retry.

### 2.7 LLM summarization call

Goose uses `complete_fast()` (fast model → fallback).  Goop will use rig's
non-streaming `CompletionModel::completion_request()` API:

```rust
// AnyAgent gains a summarize() method:
let response = agent.model().completion_request(summary_prompt)
    .preamble(compaction_system_prompt)
    .send()
    .await?;
let summary = extract_text(&response.choice);
```

The rig `Agent` struct exposes `model: Arc<M>`, so we can call
`completion_request()` directly for a one-shot non-streaming completion.  The
summarization model is configurable (separate from the main model) — see
config below.

The compaction system prompt should be a detailed template (like goose's
`compaction.md`) instructing the LLM to preserve technical content, file names,
code, errors, decisions, and pending tasks.  This template should be embedded
at compile time.

### 2.8 Context snapshots — audit trail of LLM inputs

`ContextSnapshot { seqs, model }` is recorded before each LLM call.  It lists
the seqs of events that are agent-visible at that point (post-compaction,
post-overlay), plus the model that is about to see them.

**Why seqs, not full messages:** full `Vec<Message>` at every LLM call is
expensive — a 50-turn conversation with tool calls could have 100+ LLM calls,
each with a growing context.  But we don't need the full messages.  The
snapshot records *which events formed the context* and *which model received
them*; the log holds the immutable *content*.  Together they're sufficient for
reproduction.

**Why include the model:** reproducing a past context requires knowing which
model's API constraints applied — context window size, tool-call format,
max-tokens limits.  The model also identifies whose constraints govern
formatting if that ever needs to be reproduced exactly.  And it makes the
audit trail self-contained: each snapshot says "the LLM saw these events,
and it was model X."

**What this enables:** if the replay rules (compaction selection, message
construction) change in the future, you can compare what the new rules *would*
produce against what the snapshot *did* produce.  If they differ, you know the
rule change would affect past contexts.  The snapshot is an immutable audit
trail of the selection + model; the log is an immutable record of the content.

**Why not full messages (for now):** the only thing full messages add is
freezing the *formatting*.  But formatting changes are rare and usually
backward-compatible (e.g., adding a new content type).  If exact byte-level
reproduction is ever needed, the snapshot infrastructure is already in place —
upgrade to recording full messages later with no format migration.

**Interaction with forking:** each branch has its own `ContextSnapshot` events
(they're just log entries with parent pointers like everything else).  When you
fork from a past point, you replay the active branch up to the fork point to
derive the starting context.  The snapshot at the fork point is an audit record
of what the *original* turn saw; the fork's first turn creates its own snapshot.

### 2.8a Model switches — recording mid-conversation model changes

The session's effective model is normally session-level `Config`, overridable
per-session in `<name>.state.toml`.  But a user may switch models
mid-conversation (edit config + restart, or a future `/model` command).  The
transaction log records this in two complementary ways:

**Per-call model identity** — `ContextSnapshot.model` records which model saw
each context.  This is always present, even if the model never changes.  It
gives you per-LLM-call granularity: "turn 7 was deepseek-v4-pro, turn 8 was
claude-sonnet-4-6."

**Switch boundaries** — `ModelChanged { from, to }` is appended when the
session's effective model changes.  This is present only when switches happen.
It lets the UI annotate "model switched from X to Y here" and lets future
analysis correlate quality changes with model changes.  It does not change
replay visibility — it's a metadata marker, skipped during agent-visible
message construction (like `ContextSnapshot` and `Thinking`).

**What does NOT change on model switch:**
- Replay visibility logic — `ModelChanged` is metadata, skipped during
  agent-visible message construction.
- `Compacted`/`ToolSummarized` — these already carry `model` fields recording
  which model produced each summary.  A summary generated by model A remains
  valid when the session switches to model B; the LLM just sees it as a user
  message containing a summary.
- The agent's preamble/system prompt — rebuilt from the current config when
  the agent is reconstructed.  The log doesn't record preamble text, but
  `ContextSnapshot` records the seqs, so the preamble could be reconstructed
  if needed.

**Compaction across model switches:** if the model changes after a
compaction, the next compaction summarizes `[previous_summary (from model A),
…new messages]` using model B.  The recursive summary naturally incorporates
the prior summary's content regardless of which model produced it — it's just
text in the agent view.

### 2.9 Forking / branching

Forking lets the user edit a past message and restart from there — like
ChatGPT's edit-and-regenerate.  The old branch is preserved; the new branch
diverges from the edit point.

**Mechanism:** every event has a `parent: Option<u64>`.  To fork:
1. The user edits a past user message (or types a new one at a past point).
2. A new `UserPrompt` entry is appended with `parent` set to the seq *before*
   the edited message — branching from that point.
3. The active tip is updated to the new entry.
4. Subsequent events (assistant text, tool calls, etc.) extend the new branch
   with linear parents.

```
seq 1  UserPrompt "Read the file"           parent: None
seq 2  AssistantText "I'll read it..."      parent: 1
seq 3  ToolCall read {path:"config.rs"}     parent: 2
seq 4  ToolResult "file contents..."        parent: 3
seq 5  AssistantText "Here's what I found"  parent: 4
─── user edits seq 1 → "Read config.rs and explain it" ───
seq 6  Edited { target: 1, replacement: Text("Read config.rs and explain it") }
                                                parent: None   ← branches from root
seq 7  ContextSnapshot { seqs: [6], model: "..." }    parent: 6
seq 8  AssistantText "Let me read it..."    parent: 7
seq 9  ToolCall read {path:"config.rs"}     parent: 8
...
```

The old branch (1→2→3→4→5) stays in the log untouched.  The new branch
(6→7→8→9) starts from the same root.  Replay walks backward from the active
tip to the root, collecting events along the way.

This is git's model: commits have parents, branches are just tips, forking is
creating a new commit with the same parent.  Fully transactional — nothing is
ever rewritten, old branches are preserved, switching branches just changes the
tip.

**Granularity:** parent pointers go on every `LogEntry`, so every event has
one.  But forking only makes sense at **turn boundaries** (user messages) —
you don't fork mid-stream.  The parent pointer on non-turn-start events is
always `seq - 1` (linear within a turn).  This keeps the model uniform without
encouraging weird granularity.

**UI pattern:** at a branch point, show a `< 1/2 >` indicator letting the user
switch between versions.  The main view always shows the active branch.

**Scope:** the log format supports forking from day one (parent pointers on
every entry).  The forking UI (edit-and-regenerate, branch switching) is a
later phase.  Until then, `parent` is always `Some(seq - 1)` and the active tip
is always the last seq.

### 2.10 Edit/delete overlays — writing into the LLM's mind

Edit and delete are overlay events that modify the agent's view **without
branching**.  You're changing what the LLM "remembers," then continuing forward
on the current branch.  This is an advanced prompt-engineering technique: if
you edit what an LLM "said," it's like writing directly into its mind — you
feed the prompt back and it will respond with the idea you gave it.

**Edit:** `Edited { target, replacement }` replaces the content of a prior
event in the agent view.  The original stays in the log; replay uses the
replacement.  Multiple edits to the same target: last one wins, all
intermediates preserved.

**Delete:** `Deleted { target }` hides a prior event from the agent view.
Original preserved; replay skips it.

**How it composes:**

| With… | Behavior |
|-------|----------|
| Compaction | Compaction operates on the *post-overlay* agent-visible set. A deleted message is already gone — the next compaction's `covers` won't include it. An edited message that's later compacted generates a summary from the *edited* content. |
| `ContextSnapshot` | Snapshots are historical — they record what the LLM saw *at that point*.  A later edit doesn't retroactively change old snapshots.  Sequential replay handles this naturally (edits after a snapshot don't apply when reproducing that snapshot). |
| Forking | Overlays are events with parent pointers, so they're branch-specific.  An edit on branch A doesn't affect branch B (which diverged earlier).  If the edited message is on the shared trunk, only the branch that includes the edit event sees it. |
| UI tree view | Deleted messages show faded/struck-through.  Edited messages show edited content with an optional "show original" affordance.  Both derived from replay. |

**Tool call pair handling:** deleting a `ToolCall` should also delete its
`ToolResult` (and vice versa) — otherwise the replay produces an orphaned call
or result, which some provider APIs reject.  The UI emits both `Deleted` events
explicitly; the replay's `drop_orphaned_tool_pairs` safety net (§2.5) handles
edge cases.

This is more powerful than goose — goose can hide messages (visibility flags)
but can't edit their content.  Our overlay model supports both.

### 2.11 Manual range compaction — future extensibility

The design makes manual range compaction a natural extension, not a redesign.
`Compacted.covers` is an arbitrary `Vec<u64>`, not a prefix marker.
Auto-compaction happens to cover "everything agent-visible before the latest
user message," but the data model doesn't encode that assumption.  A manually
selected range `[seq 12..seq 34]` is just a different `covers` value — same
event type, same replay logic, same UI rendering.  The `manual` flag lets the
UI label it differently.

What manual compaction would require to add later:

| Piece | Work |
|-------|------|
| `ClientMessage` variant | New `CompactRange { covers: Vec<u64> }` (or `from`/`to` seqs) |
| Server handler | Collect agent-visible messages in range, call LLM summarization, append `Compacted { manual: true, .. }` — same code path as auto-compaction |
| Replay logic | **No change** — already generic |
| UI rendering | **No change** — already generic; just add range-selection UX (shift-click / drag-select) |

### 2.12 Configurable tool-call summarization

```toml
[compaction]
# Full-conversation compaction: summarize the prefix when agent-visible
# tokens exceed this fraction of the context window.
# 0.0 or >= 1.0 disables.  Default: 0.8
auto_compact_threshold = 0.8

[compaction.tool_summarization]
# Summarize individual tool call+result pairs to save tokens.
enabled = true
# Model to use for summarization (provider/model format).
# If omitted, uses the session's main model.
model = "deepseek/deepseek-v4-flash"
# Trigger: start summarizing when tool-call count exceeds this.
# If omitted, auto-scaled from context limit (like goose).
# trigger_tool_count = 15
```

Env vars: `GOOP_COMPACTION_THRESHOLD`, `GOOP_TOOL_SUMMARIZATION`,
`GOOP_TOOL_SUMMARIZATION_MODEL`, `GOOP_TOOL_SUMMARIZATION_TRIGGER`.

### 2.13 Tree-like UI view

The web UI shows all events on the active branch.  Compaction events create
tree nodes; edits and deletes are rendered inline:

```
[User Prompt 1]
[Assistant: I'll read the file...]
┌─ ▸ [Summary: User asked to read config.rs...] ──────┐
│ [ToolCall: read] → [ToolResult: file contents...]  │  ← hidden by default
│ [ToolCall: replace] → [ToolResult: success]        │
└────────────────────────────────────────────────────┘
[User Prompt 2]
┌─ ▸ [Summary: The conversation so far covers...] ────┐
│ ┌─ ▸ [ToolSummary: searched codebase...] ─────────┐ │  ← nested (recursive)
│ │ [ToolCall: grep] → [ToolResult: matches...]    │ │
│ └──────────────────────────────────────────────────┘ │
│ [User Prompt 2]                                     │
│ [Assistant: Based on the search...]                 │
└──────────────────────────────────────────────────────┘
[User Prompt 3]  ✎ edited (show original ▸)
[Assistant: current response...]  ← live, uncompacted
```

- **Default:** full history (uncompacted) shown.  Compacted ranges show a faint
  outline with a `>` arrow and the summary text.
- **Click `>`:** expand to show the original messages that were summarized.
- **Nested compactions** (recursive summaries) produce nested outlines — a tree.
- `ToolSummarized` events create a smaller outline around just the tool pair.
- **Edited messages** show the edited content with a `✎` indicator and an
  optional "show original" affordance.
- **Deleted messages** show faded/struck-through (visible in the UI, hidden
  from the agent).
- **Branch points** (when forking lands) show a `< 1/2 >` version indicator.

Implementation: the `build_messages()` function in `state.rs` and the live
`dispatch()` path both need to produce a tree structure (or a flat list with
group markers).  A new `UiMessage` variant or a grouping wrapper represents
compaction boundaries.  The `<For>` component renders groups; CSS provides the
faint outline and toggle arrow.

**Terminal:** the terminal is a linear REPL and cannot easily show a tree.
Compaction events are rendered as inline notices ("Context compacted — N
messages summarized").  Tool summaries are shown inline.  Edits/deletes are
shown as inline notices.  This matches goose's terminal behavior.

### 2.14 What gets removed / replaced

| Component | Current | New |
|-----------|---------|-----|
| `<name>.messages.jsonl` | Agent memory, rewritten on compaction | **Eliminated** — agent memory derived from transaction log |
| `FileConversationMemory` | CompactingMemory + TemplateCompactor | Log-replay memory (load = replay, append = no-op) |
| `CompactionMode` (Tokens/Percent) | Token budget for eviction | Replaced by `auto_compact_threshold` (fraction of context window) |
| `rotate_messages_file()` | Backup before compaction | **Eliminated** — log is append-only, nothing to rotate |
| `preserve_committed_turns()` | Manual memory append on cancel/error | **Eliminated** — events already in log; `TurnEnded` reason controls visibility |
| `AnyAgent::append_to_memory()` | Manual memory append | **Eliminated** or repurposed |
| `FinalResponse` / `Error` / `Cancelled` events | Implicit turn-end markers; MaxTurns stringified; stream-ended misrecorded as FinalResponse | Replaced by `TurnEnded { reason: TurnEndReason }` — structured, exhaustive |
| Bare events in JSONL | `SessionEvent` per line | `LogEntry { seq, parent, ts, event }` envelope per line |
| Implicit timestamps | File line order (breaks with branching) | `ts: DateTime<Utc>` on every `LogEntry` |
| Implicit model identity | Session-level config, no per-call record | `ContextSnapshot { seqs, model }` (per-call) + `ModelChanged { from, to }` (switch boundaries) |

---

## 3. Goals / Features That MUST Be Maintained

### Core architecture
1. **Multi-session server** — `SessionManager` with `RwLock<HashMap>`, lazy
   `get_or_create`, disk discovery on startup, listing, deletion.
2. **All clients equal** — terminal, web, GUI all connect via WebSocket.  No
   client owns the session directly.
3. **WebSocket routing** — `/ws?session=<name>` routes to one session.
4. **Prompt queue** — unbounded mpsc, background `drain_queue()` processes
   serially (FIFO).
5. **Provider abstraction** — `AnyAgent`/`AnyStream` enums wrapping rig's
   type-level providers (DeepSeek, OpenAI, OpenRouter, Groq, Ollama, Anthropic,
   Zai).

### Session persistence & lifecycle
6. **Session survival across restarts** — load events, state, and config from
   disk.
7. **History replay** — `subscribe_all()` replays all past events before live
   ones; `HistoryComplete` signals the transition.
8. **Session discovery** — `SessionManager::discover()` scans
   `~/.config/goop/sessions/` on startup (minus `closed_sessions.json`).
9. **Session closing** — `DELETE /api/sessions/{name}` removes from memory,
   marks closed; disk preserved; re-open by recreating with same name.
10. **One log + state file** — the transaction log (`<name>.jsonl`) replaces
    both the events file and messages file.  The state file
    (`<name>.state.toml`) is unchanged (config overrides + CWD + transport),
    gaining an `active_tip` field when forking lands.

### Event system
11. **All current `SessionEvent` variants** — SessionInfo, SessionState,
    UserPrompt, Thinking, AssistantText, ToolCall, ToolResult, ContextUsage,
    HistoryComplete.  (ToolCall/ToolResult gain `id`; `FinalResponse`/`Error`/
    `Cancelled` are replaced by `TurnEnded { reason }`; new
    Compacted/ToolSummarized/ContextSnapshot/Edited/Deleted/ModelChanged
    variants added.)
12. **HistoryComplete** transition signal (catch-up → live).
13. **Event persistence** — append-only to disk, ahead of broadcast.
14. **ContextUsage** — `used`/`limit` token estimate emitted after each turn
    for the progress bar.
14a. **Turn-end reasons** — every `run_one` exit path emits exactly one
     `TurnEnded { reason }` with a structured `TurnEndReason`.  Nothing
     happens without being recorded.
14b. **Timestamps** — every `LogEntry` carries `ts: DateTime<Utc>`, enabling
     UI features like relative timestamps, tool-call/turn duration, and
     idle-gap display.
14c. **Model identity** — `ContextSnapshot { seqs, model }` records which
     model saw each context; `ModelChanged { from, to }` records mid-
     conversation model switches.

### Prompt processing
15. **Streaming responses** — `AssistantText` chunks via `AnyStream`.
16. **Tool call/result streaming** — `ToolCall` → `ToolResult` pairing (now
    with `id` for explicit pairing).
17. **Thinking state machine** — `TurnState { Idle, Thinking, Active }` with
    the invariant that a `Thinking` message is at the end iff state is
    `Thinking`.
18. **Cancellation via biased select** — cancel always wins over stream items.
19. **Cancellation recovery** — if tool turns completed, they're preserved (in
    the new model: they're already in the log; the `TurnEnded::Cancelled`
    reason with `prompt: None` marks the turn as agent-visible; the behavior —
    the LLM sees completed work on the next prompt — is preserved).
20. **Error recovery** — same preservation on stream errors
    (`TurnEnded::MaxTurnsExceeded` / `TurnEnded::Error`; committed work is
    agent-visible; MaxTurns shows actionable message noting work was saved).
21. **Max turns safety limit** — `default_max_turns` config.

### Config
22. **Config layering** — CLI > env > session > global > defaults (figment).
23. **Provider/model selection** — `model = "provider/model"` format.
24. **Tool group toggles** — `enabled_tool_groups`.
25. **Per-session config overrides** — `<name>.state.toml` → `config` section.
26. **Auto-generated default config** — written on first run from template.

### Tools
27. **File ops** — read, write, replace, read_html, cd.
28. **Shell** — runs in session CWD (local or remote).
29. **SSH transport** — transparent local/remote file ops and shell; persisted
    transport state; auto-reconnect on resume.
30. **Web fetch** — local-only HTTP.
31. **Computer use** — local-only screen/mouse/keyboard tools.
32. **Restart tool** — graceful self-restart after current prompt.
33. **MCP server support** — shared and per-session MCP servers.

### Memory / compaction (CHANGING — behavior preserved, mechanism replaced)
34. **Context window management** — still prevents context overflow.  Mechanism
    changes from destructive eviction to recursive LLM summary.
35. **Token estimation** — `estimated_tokens()` for the progress bar (now
    counts agent-visible messages only, post-compaction, post-overlay).
36. **Context length lookup table** — `lookup_context_length()` for resolving
    percentage thresholds and progress-bar limits.

### Web UI
37. **Session sidebar** — switch, create (+), delete (×), URL hash tracking.
38. **Message log with stable keys** — `<For>` keyed by `id`; no full re-render.
39. **Tool call bubbles** — expandable, `result`/`expanded` as `RwSignal`s
    (critical: `<For>` doesn't re-run child views for unchanged keys).
40. **Streaming text display** — live bubble, flushed to `AssistantFinal`.
41. **Markdown rendering** — marked.js + DOMPurify.
42. **STT** — speech-to-text (push-to-talk, WAV → whisper.cpp).
43. **PWA push notifications** — VAPID, service worker, `showNotification`.
44. **Touch gestures** — sidebar swipe, pull-to-refresh.
45. **Connection FSM** — `Disconnected → CatchingUp → Connected`; empty-state
    skeleton during catch-up prevents flash.
46. **Auto-reconnect** — exponential backoff after server restart.

### Terminal
47. **Rustyline REPL** — always a WS client; auto-starts server if needed.
48. **Streamdown markdown rendering** — single render pipeline via mpsc.
49. **Prompt history** — global `~/.config/goop/history.jsonl`; up/down
    navigation; sync after each response.
50. **Session name display** — `● session <name>` on start and exit.

### Other
51. **Push notifications** — `PushManager`, VAPID key pair, AES-128-GCM.
52. **STT** — whisper.cpp singleton, serial transcription, model auto-download.
53. **Graceful shutdown / restart** — `with_graceful_shutdown`, detached child
    process spawn, session state persisted beforehand.

---

## 4. Implementation Outline

### Phase 1: Log entry envelope & event enrichment
- Define `LogEntry { seq, parent, ts, event }` in `goop-shared`.
- Add `seq: u64` monotonic counter in `Session` (assigned at append time).
- Write `parent: Some(seq - 1)` always (linear; tree-walk ready for forks).
- Stamp `ts: DateTime<Utc>` on every entry at append time.
- Enrich `SessionEvent::ToolCall`/`ToolResult` with `id` field.
- Replace `FinalResponse`/`Error`/`Cancelled` with `TurnEnded { reason }`
  and `TurnEndReason` enum (every `run_one` exit path mapped).
- Add `Compacted`, `ToolSummarized`, `ContextSnapshot`, `Edited`, `Deleted`,
  `TurnEnded`, `ModelChanged` event variants to `goop-shared`.
- Eliminate `<name>.messages.jsonl`; consolidate to `<name>.jsonl`.
- Implement tree-walk `collect_branch()` (linear for now).

### Phase 2: Log-replay memory
- Rewrite `FileConversationMemory` as a log-replay memory:
  `load()` replays the log → `Vec<Message>`; `append()` = no-op.
- Implement event → `Message` reconstruction (including tool-call ID pairing).
- Implement the replay algorithm (§2.5): turn-buffering + overlays + compaction
  in one pass.
- Implement cancel-visibility: `TurnEnded::Cancelled { prompt: Some(_) }`
  drops the buffered turn; all other reasons commit it.
- Implement `drop_orphaned_tool_pairs` safety net (also catches in-flight
  tool calls from cancelled-with-work turns).
- Eliminate `preserve_committed_turns()` (events already in log; `TurnEnded`
  reason controls visibility).

### Phase 3: Context snapshots & model identity
- Emit `ContextSnapshot { seqs, model }` before each LLM call (in `run_one`).
- The snapshot records the current agent-visible seqs (post-compaction,
  post-overlay) and the model that is about to see them.
- Emit `ModelChanged { from, to }` when the session's effective model changes
  (config edit + restart, or future `/model` command).

### Phase 4: LLM summarization
- Add `AnyAgent::summarize()` method using rig's `completion_request()` API.
- Write the compaction system prompt template (embedded at compile time).
- Implement full-conversation compaction: threshold check before each prompt,
  LLM call, append `Compacted` event, emit UI event.
- Implement progressive tool-response stripping fallback.
- Preserve most-recent user message (re-add after summary).

### Phase 5: Tool-pair summarization
- Add config section (`[compaction.tool_summarization]`).
- Implement `maybe_summarize_tool_pairs()` background task (like goose).
- Append `ToolSummarized` events; protect current-turn tool calls.
- Configurable model + trigger.

### Phase 6: Config
- Replace `CompactionMode` with `auto_compact_threshold` (f64).
- Add `[compaction.tool_summarization]` section.
- Update env vars and default config template.

### Phase 7: Web UI tree view
- Update `build_messages()` and `dispatch()` to produce tree/group structure.
- Add `UiMessage` variant or grouping wrapper for compaction boundaries.
- Render faint outlines + `>` toggle arrows via CSS.
- Support nested (recursive) compaction groups.
- Render edits (`✎` indicator, show-original affordance) and deletes
  (faded/struck-through).

### Phase 8: Edit/delete overlays
- Add `ClientMessage` variants for edit/delete requests.
- Server handler: append `Edited`/`Deleted` events (deletes emit both halves
  of a tool pair).
- UI: edit affordance on messages; delete button; range selection (future).

### Phase 9: Forking (future)
- Add `active_tip` to `<name>.state.toml`.
- Implement fork on edit-and-regenerate: new `UserPrompt` with `parent` set to
  the fork point; update active tip.
- UI: branch indicator `< 1/2 >` at branch points; branch switching.
- Terminal: linear display of active branch only.

### Phase 10: Manual range compaction (future)
- Add `ClientMessage::CompactRange { covers: Vec<u64> }`.
- Server handler: collect agent-visible messages in range, call LLM
  summarization, append `Compacted { manual: true, .. }`.
- UI: range selection (shift-click / drag-select).

### Phase 11: Terminal
- Render `Compacted`/`ToolSummarized` events as inline notices.
- Render edits/deletes as inline notices.
- Update history replay to handle new event types.

### Phase 12: Tests & migration
- Unit tests for log replay → agent-visible messages (all overlay types).
- Unit tests for compaction (mock LLM, like goose's `MockProvider`).
- Unit tests for overlapping/nested compaction (`covers` includes prior
  summaries).
- Unit tests for edit/delete (last-wins, orphan safety net).
- Unit tests for turn-end reasons: every `TurnEndReason` variant produces
  correct agent-visibility (cancel-with-prompt drops the turn; cancel-without-
  prompt keeps it; MaxTurns/Error keep committed work; StreamEnded keeps all).
- Integration test: compaction preserves conversation continuity.
- Since backwards compat is broken, old `.messages.jsonl` files are ignored
  (the events log is the source of truth; sessions resume from events).
