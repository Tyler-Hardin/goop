# AGENTS.md — goop

**goop** is an AI agent REPL — a terminal and desktop GUI that wraps a DeepSeek
LLM with tools for reading, writing, and shell access.

## Architecture

```
                  ┌─────────────────────────────────┐
                  │          Session                 │
                  │    (agent + history + queue)      │
                  └──────────────┬──────────────────┘
                                 │ broadcast
                                 ▼
                  ┌─────────────────────────────────┐
                  │        Web Server (axum/WS)       │
                  │      127.0.0.1:8187               │
                  └────┬──────────┬──────────┬───────┘
                       │          │          │
                  WS   │     WS   │     WS   │
              ┌────────┐  ┌────────┐  ┌──────────────┐
              │Terminal│  │WebView │  │  Browser /    │
              │ Client │  │(wry)   │  │  Phone / etc  │
              └────────┘  └────────┘  └──────────────┘
```

All clients connect to the server via WebSocket.  The terminal is no longer
hard-wired to a local `Session` — it talks to the server just like every
other client.

- **`Session`** (`src/session.rs`) — owns the DeepSeek agent, a broadcast
  channel for events, and a FIFO prompt queue. Multiple views can submit
  prompts concurrently; the session drains them one at a time.
- **`SessionEvent`** (`src/events.rs`) — enum of all events the session
  emits: `UserPrompt`, `Thinking`, `AssistantText`, `ToolCall`,
  `ToolResult`, `FinalResponse`, `Error`, `Cancelled`.  Serialized as
  tagged JSON over the WebSocket.
- **Server** (`src/server.rs`) — axum HTTP + WebSocket server bound to
  `127.0.0.1:8187`. Serves `assets/index.html` and upgrades to WS for
  full event streaming with history replay.
- **TerminalClient** (`src/terminal.rs`) — a rustyline REPL with streamdown
  markdown rendering. Always connects to the server via WebSocket. Uses a
  single background render task (`render_loop`) that receives events and
  drives the streamdown parser + renderer.
- **Desktop GUI** (`src/main.rs` `run_gui`) — opens a native `wry` webview
  pointing at `http://127.0.0.1:8187`. The existing web UI
  (`assets/index.html`) is reused verbatim.
- **Tools** (`src/tools.rs`) — `#[rig_tool]` functions exposed to
  the LLM: `read`, `read_html`, `replace`, `write`, `shell`,
  `web_fetch`, `screenshot`, `cursor_position`, `mouse_move`,
  `mouse_click`, `key_type`, `key_press`, `window_list`,
  `window_focus`, `window_get_active`, `open_url`.
- **FileConversationMemory** (`src/memory.rs`) — implements rig's
  `ConversationMemory` trait backed by a JSONL file on disk. Used when
  `--session <name>` is given so the agent's conversation memory
  survives restarts.

## Startup modes

```
goop                    terminal REPL (always WS client; auto-starts server)
goop -s <name>          resume/create named session
goop serve              headless server only
goop serve -s <name>    headless server with named session
goop gui                desktop GUI (primary if no server, else client webview)
goop gui -s <name>      GUI with named session
```

All modes print the session name on startup (`● session 20260128_001`) and
on exit (`● session closed · 20260128_001`) so you can copy/paste to resume.

On launch, `goop gui` checks whether a server is already listening on
`127.0.0.1:8187`.  If yes it opens a client webview; if no it starts the
server in-process.  `goop` (terminal) always auto-starts a server if none is
running, then connects as a WS client — it never owns the session directly.

## History

Two independent history systems:

### Prompt history (global command history)
- Lives at `~/.config/goop/history.jsonl` — JSONL format, one JSON-encoded
  string per line. JSON escaping handles multi-line prompts safely.
- **Write path:** the `Session` appends every prompt from every client
  (terminal, web, GUI) to this file via `append_prompt_to_history()`.
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
