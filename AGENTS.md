# AGENTS.md — goop

**goop** is an AI agent REPL — a terminal and web UI that wraps a DeepSeek LLM
with tools for reading, writing, and shell access.

## Architecture

```
┌──────────────┐     ┌─────────────────────┐     ┌──────────────┐
│ TerminalView │────▶│      Session         │────▶│  Web Server  │
│  (rustyline) │     │  (agent + history)   │     │  (axum/WS)   │
└──────────────┘     └─────────────────────┘     └──────────────┘
```

- **`Session`** (`src/session.rs`) — owns the DeepSeek agent, a broadcast
  channel for events, and a FIFO prompt queue. Multiple views can submit
  prompts concurrently; the session drains them one at a time.
- **`SessionEvent`** (`src/events.rs`) — enum of all events the session
  emits: `UserPrompt`, `Thinking`, `AssistantText`, `ToolCall`,
  `ToolResult`, `FinalResponse`, `Error`, `Cancelled`.
- **`TerminalView`** (`src/terminal.rs`) — a rustyline REPL with
  streamdown markdown rendering. Uses a single background render task
  that receives events and drives the streamdown parser + renderer.
- **Server** (`src/server.rs`) — axum HTTP + WebSocket server bound to
  `127.0.0.1:8187`. Serves `assets/index.html` and upgrades to WS for
  full event streaming with history replay.
- **Tools** (`src/tools.rs`) — four `#[rig_tool]` functions exposed to
  the LLM: `read`, `replace`, `write`, `shell`.

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

## Building & running

```bash
cargo build
DEEPSEEK_API_KEY=… cargo run
```

The terminal REPL starts immediately. The web server binds to
`127.0.0.1:8187` (designed to sit behind nginx with origin validation).

## Coding conventions

- Rust edition 2024.
- All async I/O uses tokio (full features).
- Tools use `anyhow::Result` via `rig::tool::ToolError`.
- `#[allow(dead_code)]` is used for items expected to be consumed by
  future views (phone, web enhancements, etc.).
- Paths in tool arguments are `std::path::PathBuf`.
