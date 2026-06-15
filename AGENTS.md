# AGENTS.md — goop

**goop** is an AI agent REPL — a terminal and desktop GUI that wraps a DeepSeek
LLM with tools for reading, writing, and shell access.

## Architecture

```
                  ┌─────────────────────────────────┐
                  │        SessionManager            │
                  │  HashMap<name, Arc<Session>>      │
                  └──────────────┬──────────────────┘
                                 │
                  ┌─────────────────────────────────┐
                  │        Web Server (axum/WS)       │
                  │      127.0.0.1:8187               │
                  │   REST: /api/sessions             │
                  │   WS:   /ws?session=<name>        │
                  └────┬──────────┬──────────┬───────┘
                       │          │          │
                  WS   │     WS   │     WS   │
              ┌────────┐  ┌────────┐  ┌──────────────┐
              │Terminal│  │WebView │  │  Browser /    │
              │ Client │  │(wry)   │  │  Phone / etc  │
              └────────┘  └────────┘  └──────────────┘
```

The server manages multiple sessions concurrently.  Each WebSocket connection
routes to exactly one session via the `?session=<name>` query parameter.
The web UI shows a session sidebar for switching between sessions.

- **`SessionManager`** (`src/session.rs`) — owns a `RwLock<HashMap<String, Arc<Session>>>`.
  Creates sessions lazily (via `get_or_create`), discovers persisted sessions from disk
  on startup, and supports listing and deletion.
- **`Session`** (`src/session.rs`) — owns the DeepSeek agent, a broadcast
  channel for events, and a FIFO prompt queue. Multiple views can submit
  prompts concurrently; the session drains them one at a time.
- **`SessionEvent`** (`src/events.rs`) — enum of all events the session
  emits: `UserPrompt`, `Thinking`, `AssistantText`, `ToolCall`,
  `ToolResult`, `FinalResponse`, `Error`, `Cancelled`.  Serialized as
  tagged JSON over the WebSocket.
- **Server** (`src/server.rs`) — axum HTTP + WebSocket server bound to
  `127.0.0.1:8187`. Serves `assets/index.html`, a REST API for session
  management (`GET/POST /api/sessions`, `DELETE /api/sessions/{name}`),
  and WS upgrade at `/ws?session=<name>` with full history replay.
- **TerminalClient** (`src/terminal.rs`) — a rustyline REPL with streamdown
  markdown rendering. Always connects to the server via WebSocket. Uses a
  single background render task (`render_loop`) that receives events and
  drives the streamdown parser + renderer. Passes its session name in the
  WS URL.
- **Desktop GUI** (`src/main.rs` `run_gui`) — opens a native `wry` webview
  pointing at `http://127.0.0.1:8187`. The existing web UI
  (`assets/index.html`) is reused verbatim. If `--session` is given, the
  session name is passed in the URL hash so the web UI pre-selects it.
- **Tools** (`src/tools.rs`) — `#[rig_tool]` functions exposed to
  the LLM: `read`, `read_html`, `replace`, `write`, `shell`,
  `cd`, `web_fetch`, `screenshot`, `cursor_position`, `mouse_move`,
  `mouse_click`, `key_type`, `key_press`, `window_list`,
  `window_focus`, `window_get_active`, `open_url`.
- **FileConversationMemory** (`src/memory.rs`) — implements rig's
  `ConversationMemory` trait backed by a JSONL file on disk. One
  per session at `~/.config/goop/sessions/<name>.messages.jsonl`.

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

The server owns a `SessionManager`, not a single session.  All sessions
share one DeepSeek client (created per-session from env).

### Session lifecycle
- **Creation:** `SessionManager::create(name)` → `Session::new(256, Some(name))`.
  The session loads events and messages from disk if files exist.
- **Discovery:** On server start, `SessionManager::discover()` scans
  `~/.config/goop/sessions/` for `*.jsonl` files, extracts session names,
  and calls `get_or_create` for each.  Existing sessions become immediately
  available.
- **Deletion:** `DELETE /api/sessions/{name}` removes the session from the
  in-memory map.  Disk files are preserved — calling `get_or_create` again
  will reload them.
- **WebSocket routing:** The WS URL is `/ws?session=<name>`.  The handler
  calls `manager.get_or_create(name)` before upgrading.  If the session
  doesn't exist, it's created (loading from disk or fresh).

### REST API
- `GET /api/sessions` — returns sorted list of session names
- `POST /api/sessions` — create a new session; body `{"name": "optional"}`
- `DELETE /api/sessions/{name}` — remove from memory

### Session working directory (CWD)

Each session has its own working directory, persisted to
`~/.config/goop/sessions/<name>.cwd` (a plain text file containing the path).
On creation the CWD defaults to the server process's CWD.

- **`cd` tool** — the LLM can change the session's CWD. The tool resolves
  paths relative to the current session CWD, supports `~` for home and `..`
  for parent, canonicalises the result, persists it to disk, and updates a
  global `SESSION_CWDS` registry.
- **`shell` tool** — runs commands with `.current_dir(session_cwd)` so file
  operations are relative to the session's directory.
- **Initial CWD** is included in the agent preamble so the LLM knows where
  it starts; subsequent changes are communicated via the `cd` tool result.
- The global `SESSION_CWDS` (a `LazyLock<RwLock<HashMap<String, PathBuf>>>`)
  and a `tokio::task_local! SESSION_ID` allow tools to find their session's
  CWD without explicit plumbing.

### Web UI sidebar
The web UI (`assets/index.html`) has a left sidebar listing all sessions.
Clicking a session disconnects the current WS and opens a new one to
`/ws?session=<clicked>`.  A "+ New session" button creates via the REST API.
Each session has a delete (×) button.  The URL hash (`#session=<name>`)
tracks the active session for bookmarking.  On mobile, the sidebar is a
slide-out drawer.

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
- Two files under `~/.config/goop/sessions/`:
  - `<name>.jsonl` — `SessionEvent` stream (JSONL). Loaded into the
    session's `history` Vec on startup so late-joining clients see past
    events via `subscribe_all()`. Appended to on every `emit()`.
  - `<name>.messages.jsonl` — LLM `Message` objects (JSONL). Managed by
    `FileConversationMemory` which implements rig's `ConversationMemory`
    trait. The agent's internal memory loads from this file before each
    prompt and appends after each successful turn — no replay or
    re-execution needed.
- When a session file exists, the agent picks up the conversation where
  it left off.

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
# Start the server in the background, then connect terminal + GUI
DEEPSEEK_API_KEY=… cargo run -- serve &
DEEPSEEK_API_KEY=… cargo run              # terminal REPL (auto-connects)
DEEPSEEK_API_KEY=… cargo run -- gui       # desktop GUI (auto-connects)

# Or just run one — it'll auto-start the server
DEEPSEEK_API_KEY=… cargo run              # terminal REPL
DEEPSEEK_API_KEY=… cargo run -- gui       # desktop GUI

# Nix (all deps included)
nix build
DEEPSEEK_API_KEY=… ./result/bin/goop serve &
DEEPSEEK_API_KEY=… ./result/bin/goop gui

# Dev shell
nix develop
```

The web server binds to `127.0.0.1:8187` (designed to sit behind nginx
with origin validation).

## Coding conventions

- Rust edition 2024.
- All async I/O uses tokio (full features).
- Tools use `anyhow::Result` via `rig::tool::ToolError`.
- `#[allow(dead_code)]` is used for items expected to be consumed by
  future views (phone, web enhancements, etc.).
- Paths in tool arguments are `std::path::PathBuf`.
- CLI args use `clap` derive (`#[derive(Parser)]`).
- Never edit `Cargo.toml` directly; use `cargo add`.
