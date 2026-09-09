# jancode — Lightweight AI Coding Agent

A minimal, single-crate Rust agentic coding CLI. Built for **DevOps engineers and
SREs** who want an AI pair-programmer that stays out of the way — **fast to
build, small on disk, low in overhead**, and easy to run anywhere a Linux
terminal exists.

**Who it's for:** DevOps teams living in Ubuntu/Debian/RHEL workspaces, remote
boxes, containers, and jump hosts. It installs as one binary (no Python, no
Node, no database), spawns a daemon on demand, and shuts itself down when idle —
so it works in ephemeral CI runners and tiny VMs where heavyweight agentic CLIs
don't fit.

## Why jancode

jancode is deliberately lean compared to heavier agentic CLIs. It strips away the bloat that inflates build times, runtime footprint, and dependency surface:

- **Single crate, no workspace** — one `cargo build` produces one binary, not a 94-crate dependency graph.
- **Plain JSON, no database** — sessions and state live in JSON files, not SQLite.
- **One HTTP client** — a single OpenAI-compatible provider client replaces 10+ vendor-specific runtimes.
- **Daemon with idle shutdown** — a lightweight server spawns lazily on demand and exits after minutes of inactivity. No permanent background process.
- **Quiet by default** — tool activity renders as one `[tool] <name> <target>`
  line; status and tool output are suppressed so the terminal stays readable.
- **Local-first, no telemetry** — your prompts and sessions never leave the box
  except the API call you configure.
- **No extras** — deliberately omits a TUI, embeddings, PDF parsing, Bedrock, and browser automation. What remains does the core job well.

## Install

For Ubuntu / Debian / Fedora / RHEL (or any Linux with a Rust toolchain):

```bash
git clone <repo-url> && cd jancode
./install.sh                          # build + install to /usr/local/bin
./install.sh --service=user           # build + install + systemd user service
./install.sh --prefix "$HOME/.local"  # user-prefix install (no sudo)
```

The script detects the OS, ensures a Rust toolchain (searches `~/.cargo/bin`
for rustup installs, or offers to install the distro `cargo`/`rust` package),
builds in release mode, installs to your prefix, writes a starter
`config.toml` into `$JANCODE_HOME` (`~/.jancode` by default), and optionally
registers a systemd service for always-on deployment.

| Flag | Meaning |
| --- | --- |
| `--prefix <dir>` | Install under `<dir>/bin` (default `/usr/local/bin`) |
| `--skip-build` | Use an existing `target/release/jancode` |
| `--service=system` | Also install `/etc/systemd/system/jancode.service` |
| `--service=user` | Also install `~/.config/systemd/user/jancode.service` |
| `--uninstall` | Remove the binary and systemd units (keeps config/state) |

Set your provider key before first use; see [Provider setup](#provider-setup).

## Build

```bash
cargo build --release
```

## Run daemon

```bash
JANCODE_HOME=/data JANCODE_RUNTIME_DIR=/run/jancode ./target/release/jancode serve
```

The daemon auto-shuts down after `server.idle_timeout_secs` (default 300s) of
inactivity, so `run`/`connect` re-spawn it lazily instead of leaving a
permanent background process.

## Usage

### Interactive Client

```bash
jancode connect
```

In interactive mode, use these commands:
- `/tools` — Toggle tool-calling (enabled/disabled).
- `/tools <message>` — Force tools ON and send `<message>` in one step (e.g.
  `/tools analyze this project`). When tools are enabled, the model is given
  discovery tools (`list_dir`, `glob`, `read`, `agentgrep`, `bash`) and
  instructed to explore the working directory itself rather than asking the
  user for file paths.
- `/session` — List saved sessions for the current folder with numbers
  (`[1]`, `[2]`, ...), including model, message count, and last-updated time.
- `/resume <number>` — Resume a session listed by `/session`. Prints its
  history, then subsequent messages continue that conversation (saved back to
  the same session file on each turn).
- `/model` — List available models with numbers and switch (the prompt shows
  the current model, e.g. `gpt-4o>`). Also accepts `/model <number>` to pick
  from the last listed catalog, `/model 0` to return to the config default,
  `/model grep <term>` / `/model search <term>` to narrow the catalog by
  substring (handy with hundreds of models), and `/model <name>` to set an
  explicit model id (warns if it isn't in the fetched catalog). The catalog is
  fetched live from the provider's `GET /models` endpoint, or taken from a
  `models` list under `[provider]` in `config.toml`. Note: some models don't
  support tool calling and every turn will error while tools are enabled.
- `/memory` — List auto-captured memory notes for the current folder.
- `/forget <number>` — Delete a memory note listed by `/memory`.
- `/mcp` — List MCP servers configured in `$JANCODE_HOME/config.toml`.
- `/mcp <message>` — Force tools ON (built-in + MCP tools) and send
  `<message>` in one step (e.g. `/mcp check disk space on server1`).
- `/mcp_tools` — Connect to each configured MCP server and list the tools it
  exposes (name + one-line description). Read-only; never executes tools.
- `/mcp_status` — Show per-server connection status: `[ok]` with the tool
  count, or `[FAILED]` with the error from the connect/initialize handshake.
- `/help` — List available slash commands.
- `/quit`, `/exit`, `/q` — End the session (handled locally, never sent to the
  model).

Tool-calling is **enabled by default** in interactive mode. Tool activity is
shown compactly as `[tool] <name> <target>` lines (e.g. `[tool] read
package.json`); full tool output is fed back to the model but suppressed from
the terminal so only the AI's final response is prominent.

### Built-in tools

jancode ships with these tools (plus any discovered via [MCP servers](#mcp-model-context-protocol-servers)):

- `list_dir` / `glob` / `read` / `agentgrep` — explore the working directory.
- `bash` — run git, cargo, tests, or any command.
- `write` / `edit` — single-file writes and exact-string edits.
- `apply_patch` — apply a git-style unified diff to **multiple files** in one
  call (create, modify, delete). The workhorse for coordinated multi-file
  changes.
- `plan` — maintain a persistent step checklist in `.jancode-plan.md`
  (`create` / `append` / `complete` / `show`) so multi-step work stays on track
  across turns.
- `git` — manage the repo in the working directory: `status`, `branch`,
  `checkout`, `diff`, `log`, `add`, `commit`, `push`, `pull`, `remote`,
  `stash`. Mutations (`add`/`commit`/`push`/`pull`/`checkout`) require
  approval; read-only actions (`status`/`log`/`diff`/`branch` list/`remote`)
  run freely.

### Approval (safe file changes)

File **modifications** (`write`, `edit`, `apply_patch`) and **reads that
resolve outside the working directory** (`read`, `list_dir`, `glob`,
`agentgrep`) require consent before the daemon executes them:

- In `jancode connect` you get an interactive prompt: `[approval] <tool> —
  <reason>` then `allow? [y/N]`. Answering `n` sends `APPROVAL_DENIED` to the
  model, which will adapt (and the file is untouched).
- Headless one-shots (`jancode run --tools`) and swarm agents auto-approve — no
  human is attached.
- `bash` and `plan` are intentionally ungated: `bash` is the all-purpose power
  tool, and `plan` only touches its own bookkeeping file. `git` gates only the
  mutating actions (`add`/`commit`/`push`/`pull`/`checkout`); read-only git
  actions run without approval.

The policy lives in `$JANCODE_HOME/config.toml` under `[server]`:

```toml
[server]
idle_timeout_secs = 300
approve_mode = "prompt"   # "prompt" | "auto" | "deny"
```

- `"prompt"` (default) — interactive sessions ask; headless sessions auto-allow.
- `"auto"` — always allow risky calls (no prompts).
- `"deny"` — always block risky calls with `APPROVAL_DENIED`.

### Automatic project memory

jancode automatically remembers durable facts you mention and re-injects them
into later turns — no manual `/remember` needed.

- **Capture**: sentences containing preference/decision markers ("we use",
  "always", "prefer", "deployed to", "in this project", ...) are auto-saved to
  `~/.jancode/memory.json`, scoped to the folder you were in.
- **Injection**: before each response, the top 5 most relevant notes for the
  current folder are scored by token overlap and appended to the system prompt
  as "Project memory".
- **Limits**: this is keyword-based, not embeddings + reranking (that's what
  jcode does). It matches repeated wording well but misses paraphrase. Notes
  only surface in the folder they came from.
- Manage with `/memory` and `/forget <number>`.

### MCP (Model Context Protocol) servers

jancode can expose tools from MCP servers to the model, alongside its built-in
tools. Supported transport: **HTTP streamable**. Legacy SSE and stdio transports
are not yet supported.

Configure servers in `$JANCODE_HOME/config.toml`:

```toml
[[mcp.servers]]
name = "ops"
url = "https://your-mcp-server.example.com/mcp"
transport = "http-streamable"
bearer_env = "OPS_MCP_KEY"   # optional: export a token, sent as Authorization: Bearer
```

- `name` — short label used in logs and `/mcp`.
- `url` — the MCP streamable-HTTP endpoint the server listens on.
- `transport` — currently only `http-streamable` (the default); `sse` is
  reserved for a future transport.
- `bearer_env` — optional name of an environment variable holding a bearer
  token. If set and exported, every request carries
  `Authorization: Bearer <token>`. The token is read by the **daemon** process,
  so export it before `run`/`connect` starts the daemon (the daemon inherits the
  environment of the command that spawned it). Omit this field to send no auth
  header.

When tool-calling is enabled, jancode connects to each configured server,
runs the `initialize` handshake, discovers its tools via `tools/list`, and
registers them so the model can call them like built-in tools. Servers that
fail to connect are skipped (logged) without breaking the request.

Quick start:

```bash
/mcp                               # list configured servers
/mcp_tools                         # list tools exposed by each server (read-only)
/mcp_status                        # per-server connection status
/mcp check disk space on server1    # send with MCP tools enabled
jancode run "..." --tools          # one-shot with MCP tools available
```

### One-shot Prompt

```bash
jancode run "Explain this code" --model <model-name> --tools
```

- `--model <model-name>` — Override the default model (e.g., `gpt-4o`).
- `--tools` — Enable tool-calling for this prompt. Like interactive mode, tool
  output is shown compactly (`[tool] ...`) and only the final response is
  printed in full.

### Swarm (multi-agent)

`connect` is the swarm entry point. Every interactive chat gets a swarm session
id you can address agents with:

```bash
# Terminal 1: start an interactive chat, note the printed swarm session id
jancode connect
# -> swarm session id: connect-... (spawn agents with `jancode swarm spawn --parent <id>`)
```

Keep that chat open — it is your swarm "hub". Spawn child agents from a
**separate terminal**. Each `swarm spawn` returns immediately; the agent runs
headless inside the daemon and reports back to its parent's chat when done.

```bash
# Terminal 2 (set SID to the id printed by your chat)
SID=connect-...
```

**Headless agents are autonomous** — they get the same agent tools
(read/write/edit/apply_patch/bash/plan/grep/glob/list_dir) and **auto-approve
every tool call**, no `[approval]` prompts. `approve_mode` only governs your
interactive `connect` chat.

```bash
# One agent does a full job
jancode swarm spawn "Create index.html with a red heading, then style.css to match" \
  --parent $SID --label builder

# Sequential chaining: spawn step 2 only AFTER step 1's [report] appears in the chat
jancode swarm spawn "Create index.html with a red heading" --parent $SID --label html-maker
# ...wait for [report] in the connect chat...
jancode swarm spawn "Create style.css with a red color rule" --parent $SID --label css-maker

# Parallel: both run at the same time
jancode swarm spawn "Create index.html with a red heading" --parent $SID --label html-maker &
jancode swarm spawn "Create style.css with a red color rule" --parent $SID --label css-maker &

# Pin a model for a specific agent (per-spawn override)
jancode swarm spawn "Refactor the parser" --parent $SID --model free-stack

# Manage swarm
jancode swarm list                       # all members
jancode swarm status --session $SID      # one member
jancode swarm dm --to $SID "Message"     # DMs surface in that chat
jancode swarm stop $SID                  # stop one member (--force outside your subtree)
```

Completion reports (and DMs/broadcasts) surface in the parent's `connect` chat
as `[report] from agent-...` / `[DM] from ...` notifications on the chat's next
read from the daemon.

**Ending swarm sessions:**

```bash
# Stop every spawned agent
for id in $(jancode swarm list | awk '/agent-/ {print $2}'); do
  jancode swarm stop --force "$id"
done

# Or kill the daemon (agents run inside it, so this ends everything at once)
pkill -x jancode      # the next `jancode connect` lazily starts a fresh daemon
```

The roster is rebuilt at daemon start from persisted session files under
`$JANCODE_HOME/sessions/`. Dead sessions accumulate there across restarts; to
clear them:

```bash
rm ~/.jancode/sessions/*.json   # or $JANCODE_HOME/sessions/*
```

## Provider setup

Set an API key for your provider:

```bash
export OPENAI_API_KEY=sk-...
```

Or edit `$JANCODE_HOME/config.toml` (defaults to `~/.jancode/config.toml`):

```toml
[provider]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"
# Optional: pin the model catalog shown by /model instead of the live GET /models
models = ["gpt-4o-mini", "gpt-4o", "gpt-5"]
```

`/model` bulk-switches the model mid-chat; each message uses the session's
current model (also honored in `run --model` and `swarm spawn --model`).

## Core mechanics

- **Unix socket IPC** (`jancode.sock`) under `JANCODE_RUNTIME_DIR` — clients and the
  daemon communicate over a local socket, no HTTP port to manage.
- **`JANCODE_HOME` sandboxing** — config and state are isolated to a single
  directory you control.
- **Lazy daemon spawn** — `run` and `connect` wake the daemon on demand, so
  there is no always-on server to start or stop manually.
- **Session persistence** — JSON sessions under `$JANCODE_HOME/sessions/` survive
  across connections, with context accumulated per `connect` session.
- **In-process swarm** — multi-agent coordination (spawn/DM/broadcast/stop/list/status)
  runs inside the daemon with no external orchestrator.
- **Soft-interrupt notifications** — completion reports from swarm agents surface
  as non-blocking notifications while you work.
