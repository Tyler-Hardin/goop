# AGENTS.md — goop

**goop** is an AI agent REPL — a terminal and desktop GUI that wraps an LLM
(via rig, supporting multiple providers) with tools for reading, writing, and
shell access.

## Architecture

```
                  ┌─────────────────────────────────┐
                  │        SessionManager            │
                  │  HashMap<name, Arc<Session>>      │
                  │  + Config                        │
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
- **Tools** (`src/tools/`) — each tool implements `rig::tool::Tool` on a
  struct that holds an `Arc<SessionState>`.  Tools are organised by group:
  `file.rs` (read, write, replace, read_html, cd), `shell.rs` (shell),
  `ssh_tool.rs` (ssh, disconnect), `web.rs` (web_fetch),
  `computer.rs` (screenshot, cursor_position, mouse_*, key_*, window_*,
  open_url).  The active set is built in `build_tools()` based on
  `Config::enabled_tool_groups` and passed to the agent via the builder's
  `.tools()` method, which takes `Vec<Box<dyn ToolDyn>>`.
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
- **FileConversationMemory** (`src/memory.rs`) — implements rig's
  `ConversationMemory` trait backed by a JSONL file on disk. One
  per session at `~/.config/goop/sessions/<name>.messages.jsonl`.
- **Compaction** (`src/memory.rs`) — `SessionMemory` type alias wraps
  `FileConversationMemory` in `rig_memory::CompactingMemory` with a
  `TokenWindowMemory` policy and `TemplateCompactor`.  When the token
  budget is exceeded, older messages are evicted from the active window
  and replaced with a rolling text summary (no extra LLM call — the
  `TemplateCompactor` produces a textual rollup).  The summary cap is
  4 KiB.  Budget is configured via `compaction` in config.toml (an
  integer for absolute tokens, or a string like `"80%"` for a percentage
  of the model's context window, resolved from a built-in lookup table).
  When not set, the budget is `usize::MAX` (nothing evicted).
  Env var: `GOOP_COMPACTION`.

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
#   GOOP_MODEL             — model in provider/model format
#   GOOP_OLLAMA_BASE_URL   — Ollama API base URL (default: http://localhost:11434)

# LLM model in litellm-style provider/model format.
# Provider is the first segment, model is everything after.
# Supported providers: deepseek | openai | openrouter | groq | ollama | anthropic
model = "deepseek/deepseek-v4-pro"

# Maximum tokens per response.
max_tokens = 100000

# Maximum tool-calling turns per prompt (safety limit).
default_max_turns = 100

# Tool groups enabled for the agent.
# Available: file_ops, shell, ssh, web_fetch, computer_use
enabled_tool_groups = ["file_ops", "shell", "ssh", "web_fetch"]

# Token budget for context compaction — when the conversation exceeds this
# many tokens, older messages are evicted and replaced with a rolling text
# summary.  Remove or comment out to disable (unlimited context).
# compaction_token_budget = 64000

# Base URL for the Ollama API.  Only used when provider is ollama.
# Uncomment and set if Ollama runs on a nonstandard port or remote host.
# ollama_base_url = "http://localhost:11434"
```

Environment variables override the config file:
- `GOOP_MODEL` — model in `provider/model` format (e.g. `openai/gpt-4o`, `openrouter/openai/gpt-4o`)
- `GOOP_OLLAMA_BASE_URL` — Ollama API base URL (overrides the `ollama_base_url` config field)
- `GOOP_COMPACTION_TOKEN_BUDGET` — token budget for context compaction
- Provider-specific API keys: `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`,
  `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `ANTHROPIC_API_KEY`
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
- **Persistence:** Each session stores three files:
  - `<name>.jsonl` — event stream (JSONL)
  - `<name>.messages.jsonl` — LLM conversation memory (JSONL)
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
