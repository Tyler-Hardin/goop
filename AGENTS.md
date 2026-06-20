# AGENTS.md — goop

**goop** is an AI agent REPL — a terminal and desktop GUI that wraps an LLM
(via rig, supporting multiple providers) with tools for reading, writing, and
shell access.

## Architecture

### Workspace structure

```
goop/                         (workspace root)
├── crates/
│   ├── goop-shared/          shared types (SessionEvent, ClientMessage, PromptSource)
│   ├── goop-server/          main binary ("goop") — axum server, tools, terminal, GUI
│   │   └── assets/           embedded fallback HTML (fb.html, used when trunk dist absent)
│   └── goop-web/             Leptos frontend (built by Trunk → wasm)
│       ├── src/
│       │   ├── components/   UI components (header, message_log, input_bar, etc.)
│       │   ├── state.rs      AppState — global reactive state (RwSignals)
│       │   ├── ws.rs         WebSocket connection + SessionEvent dispatch
│       │   ├── markdown.rs   Markdown → HTML via marked.js + DOMPurify
│       │   ├── stt.rs        Speech-to-text bridge (→ js/stt.js)
│       │   ├── pwa.rs        Service worker + push subscription
│       │   └── app.rs        Root component + layout
│       ├── js/stt.js         MediaRecorder + WAV encoder (hybrid JS/Rust)
│       ├── style.css         Full app stylesheet
│       ├── index.html        Trunk entry point
│       └── Trunk.toml
└── flake.nix
```

### Data flow

```
                  ┌─────────────────────────────────┐
                  │        SessionManager            │
                  │  HashMap<name, Arc<Session>>      │
                  │  + Config  + PushManager         │
                  └──────────────┬──────────────────┘
                                 │
                  ┌─────────────────────────────────┐
                  │        Web Server (axum/WS)       │
                  │      127.0.0.1:8187               │
                  │   REST: /api/sessions             │
                  │         /api/vapid-public-key      │
                  │         /api/push-subscribe        │
                  │   WS:   /ws?session=<name>        │
                  └────┬──────────┬──────────┬───────┘
                       │          │          │
                  WS   │     WS   │     WS   │
              ┌────────┐  ┌────────┐  ┌──────────────┐
              │Terminal│  │WebView │  │  Browser /    │
              │ Client │  │(wry)   │  │  Phone / etc  │
              └────────┘  └────────┘  └──────┬───────┘
                                             │
                                    Push notification
                                    (background / locked)
```

The server manages multiple sessions concurrently.  Each WebSocket connection
routes to exactly one session via the `?session=<name>` query parameter.
The web UI shows a session sidebar for switching between sessions.

- **`SessionManager`** (`src/session.rs`) — owns a `RwLock<HashMap<String, Arc<Session>>>`
  and a `Config`. Creates sessions lazily (via `get_or_create`), discovers
  persisted sessions from disk on startup, and supports listing and deletion.
- **`Session`** (`src/session.rs`) — owns an `AnyAgent` (multi-provider agent
  abstraction), a broadcast channel for events, and a FIFO prompt queue.
  Multiple views can submit prompts concurrently; the session drains them
  one at a time.
- **`Config`** (`src/config.rs`) — provider selection, model name, tuning
  knobs, and tool-group toggles.  Owns `home_dir` (computed once at startup)
  so no other module reads `HOME` from the environment.  Loaded via
  [`figment`](https://crates.io/crates/figment) with layered precedence:
  CLI flags > env vars > session config > global config > defaults.  Global
  config lives at `~/.config/goop/config.toml`; per-session overrides live
  in `<name>.state.toml` via [`SessionConfig`].  If no global config file
  exists, a well-commented default is written automatically.
- **`model`** (`src/model.rs`) — provider abstraction layer. Wraps rig's
  type-level providers (DeepSeek, OpenAI, OpenRouter, Groq, Ollama,
  Anthropic) behind enums so one binary works with any provider. The
  `AnyAgent` enum owns the rig `Agent`; `AnyStream` unifies the
  provider-specific stream types via mapping.  Tools are attached at build
  time via `AgentBuilder::tools(Vec<Box<dyn ToolDyn>>)`, enabling runtime
  configuration of which tool groups are active.
- **`SessionEvent`** (`src/events.rs`) — enum of all events the session
  emits: `UserPrompt`, `Thinking`, `AssistantText`, `ToolCall`,
  `ToolResult`, `FinalResponse`, `ContextUsage`, `Error`, `Cancelled`.
  Serialized as tagged JSON over the WebSocket.  `ContextUsage`
  (approximate `used`/`limit` token counts) is emitted after each turn
  so the web UI can show a context-window progress bar.
- **Server** (`src/server.rs`) — axum HTTP + WebSocket server bound to
  `127.0.0.1:8187`. Serves the Leptos frontend from disk (trunk dist)
  with an embedded `assets/fb.html` as fallback, a REST API for session
  management (`GET/POST /api/sessions`, `DELETE /api/sessions/{name}`),
  and WS upgrade at `/ws?session=<name>` with full history replay.
  Supports graceful self-restart: the `restart` tool sets a flag on
  `SessionState`; after the current prompt completes, the drain loop
  fires a global shutdown signal.  `axum::serve` uses
  `with_graceful_shutdown` to close the TCP listener cleanly, then
  spawns the new binary as a detached child process before returning.
  This lets the agent modify the source, run `cargo build`, call
  `restart`, and have the changes take effect without losing any
  session state (everything is persisted to disk beforehand).
- **TerminalClient** (`src/terminal.rs`) — a rustyline REPL with streamdown
  markdown rendering. Always connects to the server via WebSocket. Uses a
  single background render task (`render_loop`) that receives events and
  drives the streamdown parser + renderer. Passes its session name in the
  WS URL.
- **Desktop GUI** (`src/main.rs` `run_gui`) — opens a native `wry` webview
  pointing at `http://127.0.0.1:8187`. Uses the Leptos frontend when
  trunk-built, falling back to the embedded `assets/fb.html`. If
  `--session` is given, the session name is passed in the URL hash.
- **Leptos frontend** (`crates/goop-web/`) — Trunk-built WASM app. Reactive
  state via `AppState` (RwSignals), WebSocket dispatch to `UiMessage` enum,
  markdown rendering via marked.js/DOMPurify, speech-to-text with hybrid
  JS/Rust, PWA push subscription. Touch gestures (sidebar swipe,
  pull-to-refresh) use raw web-sys event listeners.

  See `crates/goop-web/leptos_style_guide.md` for the Leptos coding
  conventions (FSMs over booleans, hydration, layout stability, etc.).

  **Frontend state machines:** Two explicit FSMs prevent "impossible state"
  bugs:
  - `BtnState` (`components/input_button.rs`) — unified send/mic/cancel
    button: `Idle | Recording | CancelSlide | Running | Disabled`.  All
    transitions go through pure methods; DOM handlers never set the state
    directly.
  - `ConnectionState` (`state.rs`) — WebSocket lifecycle:
    `Disconnected | CatchingUp | Connected`.  Replaces a pair of implicit
    booleans (`connected` + `catching_up`).  During `CatchingUp` the
    `EmptyState` component hides its hint text and acts as a layout-stable
    skeleton, preventing a flash-before-history.

  **ToolResult routing** (`state.rs` `dispatch()`) — the live `ToolResult`
  handler does NOT store a message index (the former `last_tool_idx` was
  fragile: `build_messages` didn't set it, so after history replay or
  session switches it could point to a wrong or nonexistent message).
  Instead it scans the `messages` vec backwards for the most-recent
  `ToolCall` whose `result` signal holds `None`.  The server serialises tool
  execution, so the first `None`-result `ToolCall` from the end is always
  the right target.  The scan is O(n) but stops at the first match,
  effectively O(1) in practice.

  **ToolCall fields must be signals** (`state.rs`, `components/message.rs`)
  — the `UiMessage::ToolCall` variant carries `result` and `expanded` as
  `RwSignal`s, not plain values.  This is intentional, not over-engineering:
  `<For>` in `message_log.rs` keys items by `id` and never re-runs the
  child view for an unchanged key.  The `ToolResult` event arrives *after*
  the `Message` component for that `ToolCall` has already been rendered, so
  a by-value `result: Option<String>` would never update in the DOM (the
  expanded bubble would always be empty).  Using a signal lets the view
  update reactively regardless of `<For>` reconciliation.  Any new field on
  `ToolCall` that is populated by a later event must follow the same
  pattern.

  **TurnState FSM** (`state.rs`) — `TurnState { Idle, Thinking, Active }`.
  Replaces the former `thinking: bool` + `remove_last_thinking()` pattern.
  The invariant: a `UiMessage::Thinking` is at the end of `messages` **iff**
  the state is `Thinking`.  Every transition out of `Thinking` must pop
  that message; every transition into `Thinking` must push one.  Both the
  live `dispatch()` path and the pure `build_messages()` history path use
  the same `leave_thinking()` helper — no more duplicated removal logic.
  After `HistoryComplete`, the initial live state is derived from the last
  message in the batch, so the FSM is always in sync with the message list.

  **Compaction tree view** (`state.rs`, `components/message.rs`) — Phase 7 of
  the compaction redesign.  `Compacted` and `ToolSummarized` events no longer
  render as one-line notices; they wrap their covered messages in collapsible
  `UiMessage::CompactedGroup` / `ToolSummaryGroup` tree nodes (faint outline,
  `▸` arrow, summary header, children hidden by default).  Nesting is
  structural: a later `Compacted` groups an earlier `CompactedGroup` as a
  child, so recursive summaries form a tree with no special-casing.
  `apply_compaction` groups from the first `covers`-matched message to the
  end of the list (correct for auto-compaction's full-prefix `covers`);
  `apply_tool_summary` targets by tool-call `id` (now stored on
  `UiMessage::ToolCall`), recursing into group children.

  **Per-message seq** (`state.rs`) — every agent-visible `UiMessage` carries
  the transaction-log `seq` of its originating event.  The web client
  reproduces the server's contiguous-from-zero seq assignment by counting
  received events (`AppState::seq_counter`, reset on connect, advanced once
  per event in `build_messages` and `dispatch`).  This is what lets
  `Compacted.covers` and `Edited`/`Deleted` `target` (both seqs) resolve to
  the right message.  The invariant holds because every event the client
  receives went through `Session::emit` (append → contiguous seq); a future
  live-only event not appended to the log would need to skip the counter.
  `Edited`/`Deleted` overlays (`apply_edit`/`apply_delete`) search the
  message tree recursively; edits set an `EditOverlay` signal (replacement +
  show-original `✎` toggle), deletes set a `deleted` flag (faded
  strikethrough).  Phase 8 wires the trigger: `ClientMessage::Edit`/`Delete`
  are sent from the web UI's hover-revealed ✎/✕ action buttons; the server
  appends the overlay events which come back as live events and set the
  signals — and like all lazily-populated `UiMessage`
  state, the overlays are `RwSignal`s (the `<For>`-keyed constraint above).

- **Tools** (`src/tools/`) — each tool implements `rig::tool::Tool` on a
  struct that holds an `Arc<SessionState>`.  Tools are organised by group:
  `file.rs` (read, write, replace, read_html, cd), `shell.rs` (shell),
  `restart.rs` (restart), `ssh_tool.rs` (ssh, disconnect), `web.rs` (web_fetch),
  `computer.rs` (screenshot, cursor_position, mouse_*, key_*, window_*,
  open_url).  The active set is built in `build_tools()` based on
  `Config::enabled_tool_groups` and passed to the agent via the builder's
  `.tools()` method, which takes `Vec<Box<dyn ToolDyn>>`.
- **STT** (`src/stt.rs`) — speech-to-text via whisper.cpp, loaded locally.
  A server-level singleton (`SpeechToText`) wraps a Whisper model (default:
  `base`, ~142 MB, auto-downloaded from HuggingFace on first use and cached
  in `~/.config/goop/models/whisper/`).  Transcription is batch-only
  (push-to-talk): the web UI sends a complete WAV file as a binary WS frame;
  `Session::submit_audio` transcribes it and submits the resulting text as a
  normal prompt.  STT is opt-in — set `[stt] enabled = true` in config.toml.
  whisper.cpp contexts are not `Sync`, so transcription is serialised behind
  a tokio `Mutex` (contention is negligible — prompts are already serial).
  WAV parsing uses `hound`; resampling is linear.
- **`SessionState`** (`src/session_state.rs`) — runtime per-session shared
  mutable state: `name`, `home_dir` (from Config), `cwd` (Mutex<PathBuf>),
  `transport` (Mutex<Transport>), and a `state_path` for persistence.  Tools
  access it through `Arc<SessionState>` — no global lookups needed.  CWD
  and transport changes are persisted to `<name>.state.toml` via `save()`.
  **`PersistedSessionState`** (same module) is the on-disk JSON snapshot:
  `config` (session overrides), `local_cwd`, and `transport` (local vs. SSH
  destination + remote CWD).  Replaces the former `.cwd`, `.cwd.local`, and
  `.ssh` scattered files.
- **SSH** (`src/ssh.rs`) — connection logic: parses `~/.ssh/config`,
  resolves `HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump`, loads
  private keys, and connects with key-first-then-password authentication.
  Exposes `ssh_connect()` for use by the `ssh` tool.
- **Transport** (`src/transport.rs`) — `Transport` enum (`Local` / `Ssh`),
  `SshState`, `SshHandler`, and `PersistedTransport` (the serializable
  snapshot).  `Transport::to_persisted()` converts a live transport into
  its persistable form.  File and shell tools route through the transport
  so they work transparently on local or remote hosts.
- **TransactionLog** (`src/memory/transaction_log.rs`) — the append-only
  log struct with **private fields** (`entries`, `next_seq`, `path`).
  `open(path, name)` is the RAII constructor: loads from disk (with legacy
  migration), injects `SessionInfo` if absent, persists if new.  `append()`
  is the **sole mutation path** — assigns seq, computes parent, stamps ts,
  all under the caller's lock.  `persist()` does best-effort async file
  write.  Keeping `next_seq` inside the struct (not a separate `AtomicU64`
  on `Session`) makes the ordering invariant (seq == parent == file order)
  structural, so a future background appender (tool-pair summarizer) can't
  corrupt the tree by racing the lock.
- **LogReplayMemory** (`src/memory/mod.rs`) — implements rig's
  `ConversationMemory` trait by **replaying the session's append-only
  transaction log** into `Vec<Message>` (the agent view). The events log
  (`~/.config/goop/sessions/<name>.jsonl`) is the single source of
  truth — the old separate `<name>.messages.jsonl` is eliminated.
  `load()` replays; `append()`/`clear()` are no-ops (the session writes
  every event to the log during streaming). The log is shared
  (`Arc<Mutex<TransactionLog>>`) between the session and the memory. See
  `docs/compaction-redesign.md`.
- **Log replay** (`replay_visible` in `src/memory/replay.rs`) — turns are
  buffered and committed only at a `TurnEnded` event;
  `TurnEnded::Cancelled { prompt: Some(_) }` drops the turn (no work
  committed), every other reason commits it. A trailing in-progress turn
  is dropped (rig appends the current prompt itself, so replay omits it).
  An orphan-tool-pair net drops any `ToolCall` whose `ToolResult` is
  absent. Replay is a **pure projection** (takes `&[LogEntry]`, returns
  `Vec<Message>`) — kept separate from `TransactionLog` for independent
  testability and to respect the distinction between the source of truth
  and its consumer-specific projections. Legacy pre-redesign events
  (removed `FinalResponse`/`Error`/`Cancelled`; `ToolCall`/`ToolResult`
  with no `id`) are migrated on load (in `transaction_log.rs`) — turn-end
  variants map to `TurnEnded` and tool calls/results get order-paired
  synthetic ids.
- **Compaction** (`src/memory/compaction.rs` + `src/session.rs`) — when the
  agent-visible conversation exceeds a token threshold, the whole prefix
  is summarized into a rolling `Compacted { summary, model, covers,
  manual }` event before the next turn. Replay applies it: the covered
  items (by seq) are dropped and the summary inserted. Summaries are
  themselves agent-visible, so later compactions summarize the prior
  summary (a rolling summary). The threshold comes from `compaction` in
  config.toml (`CompactionMode::Tokens(n)` or `Percent(pct)` of the
  model's context window); `None` disables it — **opt-in (default off)**.
  Env: `GOOP_COMPACTION`. Summarization is a one-shot, tool-less,
  memory-less completion (`AnyAgent::summarize`) with an embedded system
  prompt. A failed summarization is logged and skipped (full history kept).
  The pure decision logic (`compaction_covers`) is in `memory/compaction.rs`
  and unit-tested; `session.rs`'s `maybe_compact` is thin glue (snapshot →
  decide → LLM call → emit).
- **Tool-pair summarization** (`src/memory/compaction.rs` + `src/session.rs`
  + `src/memory/replay.rs`) — tier-1 compaction: verbose individual tool
  call+result pairs are summarized by an LLM into `ToolSummarized { id,
  summary, model }` events, reclaiming tokens incrementally without a full
  context rewrite. `maybe_summarize_tool_pairs()` runs between prompts in
  `drain_queue` (alongside `maybe_compact`), using a snapshot → summarize
  (outside lock) → revalidate → commit lifecycle. The pure decision logic
  (`select_tool_summary_candidates` — trigger check, most-recent-turn
  protection, min-tokens filter, batch truncation; `revalidate_tool_summaries`
  — drop vanished pairs) is in `memory/compaction.rs` and unit-tested;
  `session.rs` is thin glue around the LLM calls. Replay applies
  `ToolSummarized` via `apply_tool_summary()` — content-granularity surgery
  that splices the target call/result out of merged messages (reusing the
  `drop_orphaned_tool_pairs` rebuild pattern), since replay merges
  consecutive calls into one assistant `VisibleItem` and consecutive results
  into one user `VisibleItem`. Targets by tool-call `id` (stable across
  merging), not `seq`. Config: `[tool_summarization]` in config.toml
  (`enabled`, `model`, `min_tokens`, `trigger_tool_count`); **opt-in (default
  off)**. Env: `GOOP_TOOL_SUMMARIZATION*`. A separate `AnyAgent` is built via
  `build_summarizer()` when `model` is set; otherwise the session's main agent
  is used. The most-recent turn's tool calls are protected from summarization.
- **Context snapshots** (`src/session.rs`) — before each turn the session
  emits `ContextSnapshot { seqs, model }`, recording which events formed
  the LLM's context. Replay skips it (audit-only metadata).

## Startup modes

```
goop                    terminal REPL (always WS client; auto-starts server)
goop -s <name>          resume/create named session
goop serve              headless server only (discovers all sessions from disk)
goop serve -s <name>    headless server, ensure <name> session exists
goop gui                desktop GUI (primary if no server, else client webview)
goop gui -s <name>      GUI with named session pre-selected
```

The terminal prints the session name on startup (`● session 20260128_001`) and
on exit (`● session closed · 20260128_001`) so you can copy/paste to resume.

On launch, `goop gui` checks whether a server is already listening on
`127.0.0.1:8187`.  If yes it opens a client webview; if no it starts the
server in-process.  `goop` (terminal) always auto-starts a server if none is
running, then connects as a WS client — it never owns the session directly.

## Multi-session design

The server owns a `SessionManager`, not a single session.  The provider and
model are chosen at startup via the global config, but each session can
override them through its `SessionConfig` in `<name>.state.toml`.

### Config layering

Configuration is merged from five layers via [`figment`](https://crates.io/crates/figment)
(highest precedence wins):

1. **CLI flags** — `--model` (via `CliOverrides`)
2. **Environment** — `GOOP_MODEL`
3. **Session config** — `<name>.state.toml` → `config` section
4. **Global config** — `~/.config/goop/config.toml`
5. **Hard defaults** — DeepSeek, `deepseek/deepseek-v4-pro`, all tool groups except `computer_use`

### Provider configuration

Configuration lives at `~/.config/goop/config.toml`.  If no config file
exists on startup, goop writes a well-commented default file before
proceeding.  The template is `assets/default_config.toml` (rendered with
Tera at runtime, embedded via `include_str!` at compile time):

```toml
# goop configuration — ~/.config/goop/config.toml
#
# Environment variable overrides this file:
#   GOOP_MODEL                        — model in provider/model format
#   GOOP_OLLAMA_BASE_URL              — Ollama API base URL (default: http://localhost:11434)
#   GOOP_COMPACTION                   — compaction budget (integer or "80%")
#   GOOP_TOOL_SUMMARIZATION           — enable tool-pair summarization ("true" or "1")
#   GOOP_TOOL_SUMMARIZATION_MODEL     — model for tool-pair summaries (provider/model format)
#   GOOP_TOOL_SUMMARIZATION_MIN_TOKENS — min tokens for a pair to be worth summarizing
#   GOOP_TOOL_SUMMARIZATION_TRIGGER    — tool-call count that triggers summarization

# LLM model in litellm-style provider/model format.
# Provider is the first segment, model is everything after.
# Supported providers: deepseek | openai | openrouter | groq | ollama | anthropic | zai
model = "deepseek/deepseek-v4-pro"

# Maximum tokens per response.
max_tokens = 100000

# Maximum tool-calling turns per prompt (safety limit).
default_max_turns = 100

# When the agent-visible conversation exceeds a token budget, the entire
# prefix is summarized by an LLM into a rolling summary before the next
# turn.  Accepts an integer (absolute tokens) or "80%" (percentage of
# the model's context window).  Uncomment to enable.
# compaction = "75%"

# Tool groups enabled for the agent.
# Available: file_ops, shell, ssh, web_fetch, computer_use
enabled_tool_groups = ["file_ops", "shell", "ssh", "web_fetch"]

# Base URL for the Ollama API.  Only used when provider is ollama.
# Uncomment and set if Ollama runs on a nonstandard port or remote host.
# ollama_base_url = "http://localhost:11434"

# Verbose tool call+result pairs are individually summarized by an LLM,
# reclaiming tokens without a full context compaction.  Independent of
# the compaction budget above.  Uncomment to enable.
# [tool_summarization]
# enabled = true
# model = "deepseek/deepseek-v4-flash"   # omit → session's main model
# min_tokens = 2000                      # only summarize verbose pairs
# trigger_tool_count = 15                # omit → default (15)
```

Environment variables override the config file:
- `GOOP_MODEL` — model in `provider/model` format (e.g. `openai/gpt-4o`, `openrouter/openai/gpt-4o`)
- `GOOP_OLLAMA_BASE_URL` — Ollama API base URL (overrides the `ollama_base_url` config field)
- `GOOP_COMPACTION` — compaction budget (integer or `"80%"`)
- `GOOP_TOOL_SUMMARIZATION` — enable tool-pair summarization (`"true"` or `"1"`)
- `GOOP_TOOL_SUMMARIZATION_MODEL` — model for tool-pair summaries (provider/model format)
- `GOOP_TOOL_SUMMARIZATION_MIN_TOKENS` — minimum tokens for a pair to be worth summarizing
- `GOOP_TOOL_SUMMARIZATION_TRIGGER` — tool-call count that triggers summarization
- Provider-specific API keys: `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`,
  `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `ANTHROPIC_API_KEY`, `ZAI_API_KEY`
  (Ollama reads `OLLAMA_API_KEY` for authentication when behind a proxy;
  Ollama base URL can also be set via the provider-level `OLLAMA_API_BASE_URL`
  env var, but `GOOP_OLLAMA_BASE_URL` and the config field take precedence)

If no config file exists and no env vars are set, goop defaults to
DeepSeek (`deepseek/deepseek-v4-pro`).

The provider abstraction lives in `src/model.rs`. Rig's providers are
type-level (each has its own `Client` and `CompletionModel` type), so
goop wraps them behind three enums:

- **`AnyAgent`** — holds a rig `Agent` for any supported provider.
  `stream_prompt()` returns `AnyStream`.
- **`AnyStream`** — wraps each provider's `StreamingResult` behind a
  single `Stream` impl. Maps `MultiTurnStreamItem<R>` items to a
  common `AnyStreamingResponse` type for the `StreamedAssistantContent::Final`
  variant (which the session ignores).
- **`AnyStreamingResponse`** — opaque holder for provider-specific
  streaming response types.

The mapping adds zero overhead in practice: `StreamingResult` is already
`Pin<Box<dyn Stream>>`, so the enum dispatch is just one extra match per
poll.  The provider arms in `build_agent()` use a local `arm!` macro —
each expands to a client constructor call plus the common `finish_agent()`
helper.  The Ollama arm is slightly different: it uses the client builder
directly to pass the configurable `ollama_base_url` (from `Config` or
`GOOP_OLLAMA_BASE_URL`), rather than the short-form `from_env()`.

### Session lifecycle
- **Creation:** `SessionManager::create(name)` → `Session::new(256, Some(name))`.
  The session loads events, messages, and state (config overrides + CWD +
  transport) from disk if files exist.  Session config overrides are merged
  into the global config before building the agent.
- **Discovery:** On server start, `SessionManager::discover()` scans
  `~/.config/goop/sessions/` for `*.jsonl` and `*.state.toml` files,
  extracts session names, and calls `get_or_create` for each **except**
  those listed in `~/.config/goop/closed_sessions.json`.  Existing sessions
  become immediately available.
- **Persistence:** Each session stores two files:
  - `<name>.jsonl` — append-only transaction log (`LogEntry` envelopes,
    JSONL): both the UI history and the agent's memory (derived by
    replay). Loaded into the session's `history` Vec on startup.
  - `<name>.state.toml` — config overrides + CWD + transport state (TOML)
- **Closing (sidebar ×):** `DELETE /api/sessions/{name}` removes the session
  from the in-memory map and adds its name to `closed_sessions.json`.  Disk
  files are preserved — the session won't reappear on restart.  To reopen,
  create a new session with the exact same name, which removes it from the
  closed list and reloads all history from disk.
- **WebSocket routing:** The WS URL is `/ws?session=<name>`.  The handler
  calls `manager.get_or_create(name)` before upgrading.  If the session
  doesn't exist, it's created (loading from disk or fresh).  If the name
  was previously closed, it's automatically un-closed.

### REST API
- `GET /api/sessions` — returns sorted list of active session names
- `POST /api/sessions` — create a new session; body `{"name": "optional"}`
- `DELETE /api/sessions/{name}` — close session (remove from memory, mark as closed in `closed_sessions.json`; disk files preserved)
- `GET /api/vapid-public-key` — return VAPID public key for push subscription
- `POST /api/push-subscribe` — register a push subscription; body `{"subscription": {...}}`

### Session working directory (CWD)

Each session has its own working directory, persisted as part of
`~/.config/goop/sessions/<name>.state.toml` (in the `local_cwd` field).
On creation the CWD defaults to the server process's CWD.

- **`cd` tool** — the LLM can change the session's CWD. The tool resolves
  paths relative to the current session CWD, supports `~` for home and `..`
  for parent, canonicalises the result, persists it via `SessionState::save()`.
- **`shell` tool** — runs commands with the session's current directory
  (local or remote) so file operations are relative to the session's directory.
- **Initial CWD** is included in the agent preamble so the LLM knows where
  it starts; subsequent changes are communicated via the `cd` tool result.
- Tools access CWD through `Arc<SessionState>` — no global registries needed.

### SSH transport

A session can operate on a remote host via the `ssh` tool.  The transport
layer (`src/transport.rs`) abstracts local vs. remote file operations so
that `read`, `write`, `replace`, `read_html`, `shell`, and `cd` work
transparently on whichever host is active.  Transport state is persisted in
`<name>.state.toml` via the [`PersistedTransport`] enum (`local` | `ssh {
destination, remote_cwd }`) and auto-reconnected when a session is resumed.

Connection logic lives in `src/ssh.rs`, which:

- **Parses `~/.ssh/config`** — resolves `Host` aliases to `HostName`,
  `User`, `Port`, `IdentityFile`, and `ProxyJump`.  Supports `=`,
  glob patterns (`*`, `?`), and multi-value `Host` lines.  `Match`
  blocks are ignored.
- **Key authentication** — tries configured `IdentityFile` keys (or
  defaults: `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, `~/.ssh/id_ecdsa`)
  via `russh::keys::load_secret_key` and `authenticate_publickey`.  Falls
  back to password if provided and keys fail.
- **ProxyJump** — connects to the jump host, opens a `direct-tcpip`
  channel to the target, then runs SSH over that tunnel via
  `russh::client::connect_stream`.  Supports multiple chained jumps
  (comma-separated in config).
- **`ssh` tool** — calls `ssh::ssh_connect()`.  If already connected
  to a different host, disconnects first.  Persists transport state
  via `SessionState::save()`.
- **`disconnect` tool** — closes the SSH connection, resets transport
  to `Local`, and restores `local_cwd` from the persisted session state.
- **`Transport` enum** (`src/transport.rs`) — `Local` or `Ssh(Arc<SshState>)`.
  `SshState` holds the russh `Handle` (for opening exec channels) and an
  `SftpSession` (for file read/write/directory ops), each behind a
  `tokio::sync::Mutex`.  The remote CWD is tracked in `SshState.remote_cwd`.
- **File tools** (`read`, `write`, `replace`, `read_html`) — read/write
  via SFTP when SSH'd, using `SftpSession::open_with_flags`.
- **`shell` tool** — opens a fresh exec channel per command, with
  `cd <cwd> && <command> 2>&1` to combine stdout/stderr.
- **`cd` tool** — uses `SftpSession::canonicalize` to resolve remote paths
  and `SftpSession::metadata` to verify directories.
- **Computer-use tools** (`screenshot`, `cursor_position`, `mouse_*`,
  `key_*`, `window_*`, `open_url`) are **local-only** — they ignore the
  transport and always operate on the local machine.
- **`web_fetch`** is also local-only (HTTP requests always go from the
  server process).
- Host key checking against `~/.ssh/known_hosts`:
  unknown hosts are learned on first use (TOFU); changed keys are rejected.

### Web UI sidebar
The web UI (`crates/goop-web/`, built by Trunk) has a left sidebar listing
all sessions. Clicking a session disconnects the current WS and opens a new
one to `/ws?session=<clicked>`. A "+ New session" button creates via the REST
API. Each session has a delete (×) button. The URL hash (`#session=<name>`)
tracks the active session for bookmarking. On mobile, the sidebar is a
slide-out drawer with touch gesture support.

### PWA push notifications

When a prompt completes (FinalResponse, Error, or Cancelled), the session
fires a push notification via the [Web Push API](https://datatracker.ietf.org/doc/html/rfc8030)
so the PWA can alert the user even when backgrounded or phone-locked.

**Architecture:**

```
Session          PushManager        Browser Push Service      PWA (sw.js)
  │                   │                    │                     │
  │  prompt done      │                    │                     │
  │ ──notify────────> │                    │                     │
  │                   │  VAPID JWT +       │                     │
  │                   │  aes128gcm POST    │                     │
  │                   │ ─────────────────> │                     │
  │                   │                    │  push event         │
  │                   │                    │ ──────────────────> │
  │                   │                    │                     │ showNotification()
```

- **`PushManager`** (`src/push.rs`) — owns a VAPID key pair (P-256, generated
  once and persisted to `~/.config/goop/vapid.toml`) and a list of registered
  push subscriptions (persisted to `~/.config/goop/push_subscriptions.json`).
  Created at server startup; threaded through `SessionManager` → `Session`.
  On `notify()`, encrypts a JSON payload (`{"session":"...","event":"..."}`)
  via AES-128-GCM (RFC 8291) using [`ring`], signs a VAPID JWT (RFC 8292)
  using `p256::ecdsa`, and POSTs to each subscription endpoint (FCM, APNs, …).
  Expired endpoints (HTTP 410) are silently dropped.
- **Client** (`crates/goop-web/src/pwa.rs`) — registers the service worker
  (`/sw.js`), requests `Notification` permission, fetches the VAPID public
  key from `GET /api/vapid-public-key`, calls `pushManager.subscribe()`, and
  sends the resulting `PushSubscription` to `POST /api/push-subscribe`.
- **Service worker** (`assets/sw.js`) — handles `push` events by showing a
  system notification tagged with the session name (so duplicates coalesce).
  On `notificationclick`, focuses or opens the PWA window and navigates to
  the relevant session.
- **Crypto** — AES-128-GCM and HKDF via [`ring`] (pure Rust + ASM, no C
  deps).  ECDH and ECDSA via [`p256`] (pure Rust).  VAPID JWT constructed
  and signed manually (minimal DER→raw signature conversion, no JWT crate).
  This avoids the `generic-array` version conflict between `russh` (v1.x)
  and the RustCrypto stack (v0.14).

**REST endpoints added:**

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/api/vapid-public-key` | Return `{"publicKey":"..."}` for `pushManager.subscribe()` |
| `POST` | `/api/push-subscribe` | Accept `{"subscription":PushSubscription}` and store |

**Persistence files added:**

| File | Format | Purpose |
|------|--------|---------|
| `~/.config/goop/vapid.toml` | TOML | VAPID private key + public key + subject |
| `~/.config/goop/push_subscriptions.json` | JSON | Array of `PushSubscription` objects |

## History

Two independent history systems:

### Prompt history (global command history)
- Lives at `~/.config/goop/history.jsonl` — JSONL format, one JSON-encoded
  string per line. JSON escaping handles multi-line prompts safely.
- **Write path:** every `Session` appends every prompt from every client
  (terminal, web, GUI) to this shared file via `append_prompt_to_history()`.
- **Read path:** the terminal loads the file on startup and after every
  response completes (`sync_history_from_file` — clear + reload). This
  picks up prompts from web/GUI clients that arrived mid-session.
- The terminal also calls `add_history_entry` locally when the user submits
  a prompt, so their own input is immediately available for up/down
  navigation without waiting for the response to finish.
- Used for up/down arrow navigation in the terminal REPL (like bash history).

### Session history (per-session persistence)
- Always active.  If `--session <name>` is given the session is stored
  under that name; otherwise a name is auto-generated as `YYYYMMDD_NNN`
  (e.g. `20260128_001`), picking the next free sequence number for today.
- One file under `~/.config/goop/sessions/`:
  - `<name>.jsonl` — the append-only transaction log (`LogEntry` envelopes,
    JSONL). Loaded into the session's `history` Vec on startup so
    late-joining clients see past events via `subscribe_all()`, and
    **replayed by `LogReplayMemory`** to derive the agent's conversation
    memory. Appended to on every `emit()`. (The old separate
    `<name>.messages.jsonl` is gone — memory is log-replay.)
- When a session file exists, the agent picks up the conversation where
  it left off (turn-end reasons control replay visibility; cancelled-
  no-work turns are dropped, committed work is kept).

## Key design decisions

- **Multi-session server.** The server manages multiple concurrent sessions
  via `SessionManager`. Each WS connection is routed to one session by name.
  Sessions are discovered from disk on startup and created lazily on connect.
- **Single render pipeline.** The terminal uses exactly one render task
  for the entire session lifetime. Every event goes through an mpsc
  channel to it. This avoids interleaving of markdown and plain text.
- **Cancel via biased select.** The LLM stream loop uses
  `tokio::select! { biased; cancel_rx => …, stream.next() => … }` so a
  queued cancel always wins.
- **Cancellation recovery.** rig only saves to `ConversationMemory` on
  `FinalResponse`, so dropping the stream mid-turn would lose the user
  prompt and all completed tool turns.  `run_one()` tracks tool calls and
  results as they stream by; when cancelled it builds `Message` pairs
  (assistant `ToolCall` + user `ToolResult`) for every completed turn
  and appends them via `AnyAgent::append_to_memory()`.
  **If at least one tool turn completed** (any ToolCall+ToolResult pair),
  the user prompt and all completed pairs are saved to memory — the next
  prompt starts fresh with the LLM seeing all completed work.  Any
  in-flight tool call (emitted but no result yet) is intentionally
  dropped.  **If zero tool turns completed** (cancelled during thinking,
  text, or before any tool result arrived), nothing is saved — two
  consecutive `User` text messages would violate some provider APIs.
  Instead the `Cancelled` event carries the prompt text and the terminal
  repopulates the input line via `readline_with_initial` so the user can
  edit and resubmit immediately.
- **Error recovery (shared with cancellation).** The same preservation
  logic runs on the **stream-error** path (`Some(Err(e))`), not just
  cancellation.  rig yields errors — most notably
  `PromptError::MaxTurnsError` — only *after* many tool turns have
  already completed, so without this an error would discard the user
  prompt and every completed tool turn.  Both early-exit paths now call
  the shared `preserve_committed_turns()` helper, which saves the user
  prompt + all completed `ToolCall`/`ToolResult` pairs to memory (and is
  a no-op when nothing completed).  The `MaxTurnsError` is surfaced with
  an actionable message noting that work was saved; other errors are
  shown verbatim.
- **Prompt queue.** `Session::submit()` sends into an unbounded mpsc;
  a background `drain_queue()` task processes them serially.
- **History replay.** `subscribe_all()` returns a `SessionSubscriber`
  that replays all past events before yielding live ones. This lets
  late-joining web clients catch up.
- **All clients are equal.** The terminal, GUI, and web clients all connect
  to the server via WebSocket.  Even when `goop` starts the server itself,
  it immediately connects as a WS client — it never owns the session
  directly.
- **GUI mode.** `goop gui` runs the server on a background tokio runtime
  (if primary) and opens a native OS webview (WKWebView on macOS,
  WebView2 on Windows, WebKitGTK on Linux).

## Building & running

```bash
# Build the Leptos frontend first (one-time, or after frontend changes).
# The server falls back to an embedded fb.html if the dist is missing.
(cd crates/goop-web && trunk build)

# DeepSeek (default)
DEEPSEEK_API_KEY=… cargo run

# OpenAI
GOOP_MODEL=openai/gpt-4o OPENAI_API_KEY=… cargo run

# OpenRouter (200+ models via one API key)
GOOP_MODEL=openrouter/openai/gpt-4o OPENROUTER_API_KEY=… cargo run

# Ollama (local)
GOOP_MODEL=ollama/llama3.2 cargo run

# Anthropic
GOOP_MODEL=anthropic/claude-sonnet-4-6 ANTHROPIC_API_KEY=… cargo run

# Start the server in the background, then connect terminal + GUI
DEEPSEEK_API_KEY=… cargo run -- serve &
DEEPSEEK_API_KEY=… cargo run              # terminal REPL (auto-connects)
DEEPSEEK_API_KEY=… cargo run -- gui       # desktop GUI (auto-connects)

# Nix (all deps included — trunk build is part of the package)
nix build
DEEPSEEK_API_KEY=… ./result/bin/goop serve &
DEEPSEEK_API_KEY=… ./result/bin/goop gui

# Dev shell
nix develop
```

The web server binds to `127.0.0.1:8187` (designed to sit behind nginx
with origin validation).

**After making frontend changes**, rebuild with `(cd crates/goop-web && trunk build)`.
The server reads the Trunk dist from disk at runtime — no `cargo build` needed,
just refresh the browser.  Back-end (server) changes do require `cargo build`
and a restart (the `restart` tool handles this when the new binary is ready).

## Coding conventions

- Rust edition 2024.
- All async I/O uses tokio (full features).
- Tools implement `rig::tool::Tool` (manually, not via `#[rig_tool]`). Each
  tool struct holds `Arc<SessionState>` for CWD/transport/home_dir access.
- `Config::home_dir` is the single source of truth for `$HOME` — computed
  via the `dirs` crate (`dirs::home_dir()`).  No module reads
  `std::env::var("HOME")`.
- Config directory is determined via `dirs::config_dir()` + `"goop"`,
  not hardcoded `~/.config/goop`.
- Tool groups are gated via `Config::enabled_tool_groups` (a `Vec<ToolGroup>`
  in `config.toml`).  `build_tools()` produces a `Vec<Box<dyn ToolDyn>>` at
  agent construction time — disabled groups simply aren't included.
- Paths in tool arguments are `std::path::PathBuf`.
- CLI args use `clap` derive (`#[derive(Parser)]`).
- Never edit `Cargo.toml` directly; use `cargo add`.

## Tool groups

```toml
# ~/.config/goop/config.toml
enabled_tool_groups = ["file_ops", "shell", "ssh", "web_fetch", "computer_use"]
```

Available groups: `file_ops`, `shell`, `ssh`, `web_fetch`, `computer_use`.
Default: all except `computer_use`.

The `shell` group includes the `restart` tool (no args — schedules a
graceful server restart after the current prompt completes).
