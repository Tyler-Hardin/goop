# goop — the effective slop generator

**goop** is an AI agent that works where you work — terminal, desktop, or
phone.  It wraps an LLM with tools for reading, writing, shell access, SSH,
and web fetching.  A persistent web server mirrors your active sessions so
you can walk away from your desk and pick up right where you left off, from
any device on your network.

```
$ goop
● session 20260202_001

> refactor the auth module to use Argon2
  thinking… ✓
  wrote src/auth.rs ✓
  wrote tests ✓
  cargo test → all green ✓
```

## Why goop?

- **Roam free.**  Start a task at your desk, continue from your phone.
  Sessions are mirrored in real time through a web UI — no syncing, no
  SSH, no screen sharing.
- **Everything is a tool.**  The agent reads and writes files, runs shell
  commands, SSHs into remote hosts, fetches web pages, and can control the
  desktop (screenshot, mouse, keyboard).  You control which tool groups are
  active.
- **Your model, your keys.**  Bring your own API keys.  Supports DeepSeek,
  OpenAI, OpenRouter (200+ models), Groq, Anthropic, Z.ai, and local Ollama.
- **Persistent by default.**  Every session is saved to disk — conversation
  history, working directory, and SSH state.  Resume any session by name.
- **One binary, three interfaces.**  Terminal REPL, desktop GUI (native
  webview), or web browser.  They all talk to the same server.

## Quickstart

### Nix (recommended)

```bash
nix build
DEEPSEEK_API_KEY=sk-… ./result/bin/goop
```

### Cargo

```bash
DEEPSEEK_API_KEY=sk-… cargo run
```

That's it.  No config file needed — goop writes a sensible default on first
run.

### Pick a different model

```bash
# OpenAI
GOOP_MODEL=openai/gpt-4o OPENAI_API_KEY=sk-… goop

# Anthropic
GOOP_MODEL=anthropic/claude-sonnet-4-6 ANTHROPIC_API_KEY=sk-… goop

# OpenRouter (200+ models, one API key)
GOOP_MODEL=openrouter/openai/gpt-4o OPENROUTER_API_KEY=sk-… goop

# Local Ollama
GOOP_MODEL=ollama/llama3.2 goop

# Groq
GOOP_MODEL=groq/llama-3.2-70b-versatile GROQ_API_KEY=gsk-… goop

# Z.ai / GLM
GOOP_MODEL=zai/glm-5.2 ZAI_API_KEY=… goop
```

## Modes

```
goop                    terminal REPL (starts server if needed)
goop -s my-project      resume or create a named session
goop serve              headless server (no UI, just HTTP + WS)
goop serve -s my-project  headless, ensure session exists
goop gui                desktop GUI (native webview)
goop gui -s my-project  GUI with session pre-selected
```

### The phone trick

Start the server on your desktop, then open `http://your-machine:8187` on
your phone.  You'll see the same sessions, the same history, and the same
agent.  Submit prompts from your phone while the agent works on your
desktop's filesystem.

The server listens on `127.0.0.1:8187` by default — put it behind nginx or
a Tailscale funnel to expose it safely.

## Configuration

Everything lives in `~/.config/goop/`.  On first run goop writes a
well-commented `config.toml`:

```toml
# ~/.config/goop/config.toml
model = "deepseek/deepseek-v4-pro"
max_tokens = 100000
default_max_turns = 100
enabled_tool_groups = ["file_ops", "shell", "ssh", "web_fetch"]
```

Environment variables override the config file:

| Variable | Purpose |
|---|---|
| `GOOP_MODEL` | Model in `provider/model` format |
| `GOOP_OLLAMA_BASE_URL` | Ollama API URL (default: `http://localhost:11434`) |
| `GOOP_COMPACTION` | Compaction budget — integer or `"80%"` |
| `GOOP_TOOL_SUMMARIZATION` | Enable tool-pair summarization (`"true"` or `"1"`) |
| `DEEPSEEK_API_KEY` | DeepSeek API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `OPENROUTER_API_KEY` | OpenRouter API key |
| `GROQ_API_KEY` | Groq API key |
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `ZAI_API_KEY` | Z.ai / GLM API key |

## Tools

goop gives the agent real access to your machine — you decide how much.

| Group | Tools | Description |
|---|---|---|
| `file_ops` | read, write, replace, read_html, cd | File I/O and directory navigation |
| `shell` | shell, restart | Run arbitrary shell commands; restart the server after recompiling |
| `ssh` | ssh, disconnect | Connect to remote hosts; file and shell tools then run remotely |
| `web_fetch` | web_fetch | Fetch and extract text from web pages |
| `computer_use` | screenshot, cursor_position, mouse_*, key_*, window_*, open_url | Desktop control (disabled by default) |

Enable or disable groups in `config.toml`:

```toml
# Full access
enabled_tool_groups = ["file_ops", "shell", "ssh", "web_fetch", "computer_use"]

# Read-only web research
enabled_tool_groups = ["web_fetch"]
```

### SSH

The `ssh` tool connects the session to a remote host.  Once connected, all
file and shell tools transparently operate on the remote machine.  goop
parses your `~/.ssh/config`, respects `ProxyJump`, and tries keys before
falling back to password auth.

```
> ssh user@staging.example.com
  connected ✓

> cat /etc/nginx/nginx.conf
  … (reads from remote) …

> disconnect
  back on local ✓
```

### Context compaction

When the conversation gets long, goop can automatically summarize the prefix
into a rolling summary — keeping the LLM focused without losing context.

```toml
# ~/.config/goop/config.toml
compaction = "75%"          # trigger at 75% of model's context window
# compaction = 64000        # or an absolute token limit
```

Set via `GOOP_COMPACTION` env var or config file.  Opt-in (default off).

Verbose tool call+result pairs can also be individually summarized (tier-1
compaction), independent of full compaction:

```toml
[tool_summarization]
enabled = true
model = "deepseek/deepseek-v4-flash"   # cheap model for summaries
min_tokens = 2000
```

### MCP (Model Context Protocol)

Connect goop to external MCP servers — their tools appear alongside goop's
built-in tools, named `server.tool`.

```toml
# ~/.config/goop/config.toml
[mcp_servers.github]
type = "http"
url = "http://localhost:8080"
shared = true            # one instance, all sessions

[mcp_servers.code_indexer]
type = "stdio"
command = "my-indexer"
args = ["--project", "."]

enabled_mcp_servers = ["github"]
```

### Speech-to-text

Opt-in local speech recognition via Whisper.  Models auto-download on first use.

```toml
[stt]
enabled = true
model = "base"            # tiny | base | small | medium | large
```

### Edit, delete, and fork

Right-click or hover any message in the web UI to edit its content (the LLM
sees the edit), delete it (and its tool-pair half), or fork the conversation
from that point — branching off a new timeline while preserving the old one.

## Sessions

Every session is persisted to `~/.config/goop/sessions/`:

- `<name>.jsonl` — append-only transaction log (full event history, replayed for both UI and LLM memory)
- `<name>.state.toml` — working directory, SSH state, and per-session config overrides

Sessions are auto-named as `YYYYMMDD_NNN` unless you give them a name with
`-s`.  Close a session from the web sidebar (× button) — disk files are
preserved, and you can reopen it later with the same name.

## Managing the server

```bash
# Start headless in the background
DEEPSEEK_API_KEY=… goop serve &

# Then connect from anywhere
goop                # terminal
goop gui            # desktop GUI
curl localhost:8187 # web UI
```

The REST API:

```
GET  /api/sessions              list active sessions
POST /api/sessions              create a new session
DELETE /api/sessions/{name}     close a session
GET  /api/vapid-public-key      VAPID public key for push notifications
POST /api/push-subscribe        register a push subscription
WS   /ws?session={name}         real-time event stream
```

## Building

```bash
# Rust
cargo build --release

# Nix (includes all system deps)
nix build
```

The desktop GUI depends on WebKitGTK (Linux), WebView2 (Windows), or
WKWebView (macOS).  The Nix build handles this automatically.

## License

MIT
