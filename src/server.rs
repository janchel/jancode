use crate::config::runtime_dir;
use crate::protocol::{Event, Request};
use crate::storage::{list_sessions, save_session, Session, Message};
use crate::swarm::{self, Interrupt, SwarmState};
use crate::tools::{is_outside, ToolContext};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

type SessionMap = Arc<RwLock<HashMap<String, Session>>>;
/// Count of currently-connected clients, shared with the idle-shutdown watcher.
type ActiveClients = Arc<AtomicUsize>;
/// Shared swarm state (in-process multi-agent coordination).
type SharedSwarm = Arc<RwLock<SwarmState>>;
/// session_id -> the owning interactive client's socket (for live push of
/// swarm notifications like DM / broadcast / completion reports).
type SessionClients = Arc<RwLock<HashMap<String, Arc<tokio::net::UnixStream>>>>;
/// Writer shared between the read loop and spawned turn tasks so a `Cancel`
/// can be processed while a turn is still streaming. Serializes event lines.
type Socket = Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>;
/// request id -> in-flight turn task, so `Request::Cancel` can abort it.
type InFlight = Arc<RwLock<HashMap<u64, tokio::task::JoinHandle<()>>>>;
/// "&lt;msg_id&gt;:&lt;tool_call_id&gt;" -> approval channel, bridging the read
/// loop (which sees `ApprovalResponse`) to the spawned turn awaiting the
/// human's decision.
type Approvals = Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>;

pub async fn run() -> Result<()> {
    let cfg = crate::config::load()?;
    let runtime = runtime_dir();
    std::fs::create_dir_all(&runtime).context("creating runtime dir")?;
    let socket_path = runtime.join("jancode.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).context("removing stale socket")?;
    }
    let listener = tokio::net::UnixListener::bind(&socket_path).context("binding unix socket")?;
    info!("jancode daemon listening on {}", socket_path.display());

    let sessions: SessionMap = Arc::new(RwLock::new(HashMap::new()));
    for s in crate::storage::list_sessions().await? {
        sessions.write().await.insert(s.id.clone(), s);
    }

    // Reconstruct the swarm roster from persisted sessions so spawned agents
    // survive daemon restarts (jancode reloads swarm state from disk).
    let swarm: SharedSwarm = Arc::new(RwLock::new(SwarmState::new("default".to_string())));
    {
        let mut sw = swarm.write().await;
        let live = sessions.read().await;
        for s in live.values() {
            let is_headless = s.title.starts_with("agent-");
            let parent = s.title.strip_prefix("agent-").map(|p| p.to_string());
            let member = swarm::make_member(&s.id, None, parent, is_headless);
            sw.register_member(member);
        }
    }

    // Idle shutdown: like jancode, the daemon exits on its own once no client is
    // connected long enough, so `run`/`connect` re-spawn it lazily instead of
    // leaving a permanent background process.
    let session_clients: SessionClients = Arc::new(RwLock::new(HashMap::new()));
    let active_clients: ActiveClients = Arc::new(AtomicUsize::new(0));
    let idle_secs = cfg.server.idle_timeout_secs;
    if idle_secs > 0 {
        let watcher = active_clients.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(idle_secs)).await;
                if watcher.load(Ordering::SeqCst) == 0 {
                    info!("idle timeout reached ({}s), shutting down", idle_secs);
                    std::process::exit(0);
                }
            }
        });
    }

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let sessions = sessions.clone();
                let swarm = swarm.clone();
                let session_clients = session_clients.clone();
                let active = active_clients.clone();
                active.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _ = handle_client(stream, sessions, swarm, session_clients).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) => error!("accept error: {}", e),
        }
    }
}

/// Reload the config from disk, falling back to the cached copy if the on-disk
/// file is missing or invalid. MCP servers are read this way on every probe
/// and every tool-enabled turn, so token/server edits take effect **without
/// restarting the daemon** (letting users fix e.g. an MCP bearer token and just
/// run `/mcp_reconnect`).
fn reload_cfg(cached: &crate::config::Config) -> crate::config::Config {
    match crate::config::load() {
        Ok(fresh) => fresh,
        Err(e) => {
            tracing::warn!("could not reload config ({}); using cached copy", e);
            cached.clone()
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    sessions: SessionMap,
    swarm: SharedSwarm,
    session_clients: SessionClients,
) -> Result<()> {
    // Duplicate the socket so swarm handlers running on other connections can
    // push Notification events (DM / broadcast / completion report) into this
    // client's chat live. Dropping the mirror never shuts down the socket.
    let std_stream = stream.into_std().context("converting to std stream")?;
    let mirror = std_stream
        .try_clone()
        .ok()
        .and_then(|dup| tokio::net::UnixStream::from_std(dup).ok())
        .map(Arc::new);
    let stream = tokio::net::UnixStream::from_std(std_stream).context("rebuilding tokio stream")?;
    let (r, w) = stream.into_split();
    let w: Socket = Arc::new(tokio::sync::Mutex::new(w));
    let mut lines = BufReader::new(r).lines();
    let cfg = Arc::new(crate::config::load()?);
    let approvals: Approvals = Arc::new(RwLock::new(HashMap::new()));
    let in_flight: InFlight = Arc::new(RwLock::new(HashMap::new()));
    // Sessions this client registered as the interactive owner for; cleaned up
    // on disconnect so stale socket handles don't accumulate.
    let mut registered_sessions: Vec<String> = Vec::new();

    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match req {
            Request::Ping { id } => {
                send(&w, &Event::Pong { id }).await?;
            }
            Request::McpProbe { id } => {
                // Reload config so /mcp_status and /mcp_reconnect reflect
                // on-disk edits (e.g. a newly added bearer token) immediately.
                let live_cfg = reload_cfg(&cfg);
                let servers = crate::mcp::probe(&live_cfg).await;
                send(&w, &Event::McpInfo { id, servers }).await?;
            }
            Request::GetHistory { id, session_id: _ } => {
                let msgs: Vec<crate::protocol::Message> = {
                    let s = sessions.read().await;
                    s.values()
                        .flat_map(|entry| entry.messages.clone())
                        .map(|m| crate::protocol::Message {
                            role: m.role,
                            content: m.content,
                        })
                        .collect()
                };
                send(
                    &w,
                    &Event::History {
                        id,
                        messages: msgs,
                    },
                )
                .await?;
            }
            Request::Message { id, session_id, content, tools, model, provider, cwd, interactive } => {
                let session_id_s = session_id.unwrap_or_else(|| format!("session-{}", id));
                let working_dir = cwd.clone().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_default().to_string_lossy().to_string()
                });

                // An interactive client is a human-attached swarm member: it can
                // act as a parent for spawned agents and receives their DMs and
                // completion reports live on its socket.
                if interactive {
                    {
                        let mut sw = swarm.write().await;
                        if sw.get_member(&session_id_s).is_none() {
                            let member = swarm::make_member(
                                &session_id_s,
                                Some("interactive".to_string()),
                                None,
                                false,
                            );
                            sw.register_member(member);
                        }
                    }
                    if let Some(m) = &mirror {
                        session_clients
                            .write()
                            .await
                            .insert(session_id_s.clone(), m.clone());
                        registered_sessions.push(session_id_s.clone());
                    }
                }

                let enable_tools = tools.is_some();
                let tool_registry = if enable_tools {
                    let mut reg = crate::tools::default_registry();
                    // Load MCP tools from a freshly-read config so edits (tokens,
                    // new servers) apply on the next turn without a daemon restart.
                    let mcp_cfg = reload_cfg(&cfg);
                    for t in crate::mcp::load_tools(&mcp_cfg).await {
                        reg.register_boxed(t);
                    }
                    Some(reg)
                } else {
                    None
                };

                // Ensure session exists and push the user message.
                {
                    let mut map = sessions.write().await;
                    let entry = map.entry(session_id_s.clone()).or_insert_with(|| Session {
                        id: session_id_s.clone(),
                        title: format!("session-{}", id),
                        working_dir: working_dir.clone(),
                        model: model.clone().unwrap_or_else(|| {
                            crate::config::resolve_provider(cfg.as_ref(), provider.as_deref()).default_model.clone()
                        }),
                        messages: Vec::new(),
                        created_at_ms: chrono::Utc::now().timestamp_millis(),
                        updated_at_ms: chrono::Utc::now().timestamp_millis(),
                        approved_paths: HashSet::new(),
                    });
                    if let Some(ref m) = model {
                        entry.model = m.clone();
                    }
                    entry.messages.push(Message {
                        role: "user".to_string(),
                        content: content.clone(),
                        timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                    entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                }

                // Automatically remember durable facts the user mentions.
                let _ = crate::memory::auto_capture(&content, &working_dir);

                send(&w, &Event::Ack { id }).await?;
                send(&w, &Event::Status { id: Some(id), message: "Thinking...".to_string() }).await?;

                // Spawn the turn as its own task so the read loop stays free to
                // accept `Request::Cancel` (aborts this turn) and
                // `Request::ApprovalResponse` (forwards to the gated tool)
                // while the provider streams. This branch returns immediately;
                // `handle_message_turn` does the heavy work.
                let turn_w = w.clone();
                let turn_sessions = sessions.clone();
                let turn_approvals = approvals.clone();
                let turn_in_flight = in_flight.clone();
                let turn_cfg = cfg.clone();
                let handle = tokio::spawn(async move {
                    match handle_message_turn(
                        &turn_w,
                        turn_sessions,
                        &turn_approvals,
                        &turn_cfg,
                        id,
                        session_id_s,
                        working_dir,
                        content,
                        tool_registry,
                        interactive,
                        provider,
                    )
                    .await
                    {
                        Err(e) => {
                            let _ = send(&turn_w, &Event::Error { id: Some(id), message: e.to_string() }).await;
                            let _ = send(&turn_w, &Event::Done { id: Some(id) }).await;
                        }
                        Ok(()) => {}
                    }
                    turn_in_flight.write().await.remove(&id);
                });
                in_flight.write().await.insert(id, handle);
            }
            Request::Cancel { id, session_id: _ } => {
                if let Some(handle) = in_flight.write().await.remove(&id) {
                    handle.abort();
                    info!("cancelled in-flight request {} (aborted turn)", id);
                    let _ = send(&w, &Event::Done { id: Some(id) }).await;
                }
            }
            Request::ApprovalResponse { id, tool_call_id, approved, .. } => {
                let key = format!("{}:{}", id, tool_call_id);
                if let Some(tx) = approvals.write().await.remove(&key) {
                    let _ = tx.send(approved);
                }
            }

            // ---- Swarm / multi-agent (jancode-style, in-process) ----
            Request::SwarmSpawn {
                id,
                parent_session_id,
                initial_message,
                model,
                provider,
                label,
            } => {
                handle_swarm_spawn(
                    &w, id, &sessions, &swarm, &session_clients, parent_session_id,
                    &initial_message, model.as_deref(), provider.as_deref(), label.as_deref(),
                )
                .await?;
            }
            Request::SwarmDM {
                id: _,
                from_session_id,
                to_session_id,
                message,
            } => {
                handle_swarm_dm(
                    &w, &swarm, &session_clients, from_session_id.as_deref(),
                    &to_session_id, &message,
                )
                .await?;
            }
            Request::SwarmBroadcast {
                id: _,
                from_session_id,
                message,
            } => {
                handle_swarm_broadcast(
                    &w, &swarm, &session_clients, from_session_id.as_deref(), &message,
                )
                .await?;
            }
            Request::SwarmStop {
                id,
                session_id,
                force,
            } => {
                handle_swarm_stop(&w, &swarm, id, &session_id, force).await?;
            }
            Request::SwarmStatus { id, session_id } => {
                handle_swarm_status(&w, &swarm, id, session_id.as_deref()).await?;
            }
            Request::SwarmList { id } => {
                handle_swarm_list(&w, &swarm, id).await?;
            }
        }
    }
    // Client disconnected: drop our socket registrations so notifications stop
    // flowing to a dead handle (the sessions stay as swarm members), and abort
    // any in-flight turns so their tasks don't linger after the writer dies.
    {
        let mut map = session_clients.write().await;
        for sid in &registered_sessions {
            map.remove(sid);
        }
    }
    for (_, h) in in_flight.write().await.drain() {
        h.abort();
    }
    Ok(())
}

/// A tool call that requires approval before executing, with a human-readable
/// reason and (where applicable) the target path for context.
struct ApprovalGate {
    path: Option<String>,
    reason: String,
}

/// Decide whether a tool call must be gated behind approval.
fn gate_tool(name: &str, input: &Value, ctx: &ToolContext) -> Option<ApprovalGate> {
    match name {
        "fetch_url" => None,
        "http_request" => {
            let method = input.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_uppercase();
            if matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
                None
            } else {
                Some(ApprovalGate {
                    path: None,
                    reason: format!("HTTP {} to {}", method, input.get("url").and_then(|v| v.as_str()).unwrap_or("<url>")),
                })
            }
        }
        "note" => None, // own bookkeeping file, like plan
        "docker" => {
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let read_only = matches!(action, "ps" | "images" | "logs" | "inspect" | "stats");
            if read_only {
                None
            } else {
                let target = input
                    .get("container")
                    .and_then(|v| v.as_str())
                    .or_else(|| input.get("image").and_then(|v| v.as_str()))
                    .unwrap_or("");
                Some(ApprovalGate {
                    path: None,
                    reason: format!("docker {} {}", action, target).trim().to_string(),
                })
            }
        }
        "sql" => {
            let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if crate::tools::sql_is_read_only(query) {
                None
            } else {
                let preview: String = query.chars().take(60).collect();
                Some(ApprovalGate {
                    path: None,
                    reason: format!("run SQL: {}", preview),
                })
            }
        }
        "git" => {
            // Read-only git actions never need approval; anything that mutates
            // the repo (add/commit/push/pull/checkout/branch -D/stash
            // save|pop|drop) requires approval in interactive mode.
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let mut mutating = matches!(
                action,
                "add"
                    | "commit"
                    | "push"
                    | "pull"
                    | "checkout"
                    | "branch"
                    | "stash"
                    | "merge"
                    | "rebase"
                    | "reset"
            );
            if action == "branch"
                && !input
                    .get("delete_branch")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                mutating = false; // plain "git branch <name>" just creates a pointer
            }
            if action == "stash" {
                let sa = input.get("stash_action").and_then(|v| v.as_str());
                mutating = !matches!(sa, None | Some("list"));
            }
            if mutating {
                let detail = input
                    .get("branch")
                    .and_then(|v| v.as_str())
                    .or_else(|| input.get("refspec").and_then(|v| v.as_str()))
                    .or_else(|| input.get("ref").and_then(|v| v.as_str()))
                    .or_else(|| input.get("message").and_then(|v| v.as_str()))
                    .unwrap_or("");
                Some(ApprovalGate {
                    path: None,
                    reason: format!("git {} {}", action, detail).trim().to_string(),
                })
            } else {
                None
            }
        }
        "write" | "edit" => {
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let full = ctx.resolve_path(p);
            Some(ApprovalGate {
                path: Some(full.display().to_string()),
                reason: format!("modify file {}", full.display()),
            })
        }
        "apply_patch" => Some(ApprovalGate {
            path: None,
            reason: "apply a multi-file patch".to_string(),
        }),
        "read" | "list_dir" | "glob" | "agentgrep" => {
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let full = ctx.resolve_path(p);
            // Heuristic: `..` inside a glob pattern can escape the base path.
            let pattern_escape = if name == "glob" {
                input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .map(|s| s.split(['/', '\\']).any(|c| c == ".."))
                    .unwrap_or(false)
            } else {
                false
            };
            if is_outside(&ctx.working_dir, &full) || pattern_escape {
                Some(ApprovalGate {
                    path: Some(full.display().to_string()),
                    reason: format!("read outside workspace ({})", full.display()),
                })
            } else {
                None
            }
        }
        "bash" => {
            // `bash` is the all-purpose power tool, but it can read/search
            // anywhere on the host. Gate it when the command references files
            // or directories outside the working directory (absolute paths,
            // `..`, `~`, `$HOME`, `cd` out of the workspace). The
            // `[server] bash_gate` config controls how aggressive this is.
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            crate::tools::bash_escapes_workspace(command, &ctx.working_dir, &ctx.bash_gate).map(|reason| {
                ApprovalGate {
                    path: None,
                    reason: format!("bash may touch outside workspace: {}", reason),
                }
            })
        }
        _ => None,
}
    }

/// Normalize a path string for use as a cache key.
/// Handles variations like "index.html", "./index.html", "dir/../index.html"
/// by resolving to a canonical form without touching the filesystem.
fn normalize_path_key(path: &str) -> String {
    use std::path::{Path, PathBuf};
    let is_absolute = Path::new(path).is_absolute();
    let mut components = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            std::path::Component::Normal(name) => components.push(name.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => { if !components.is_empty() { components.pop(); } }
            std::path::Component::CurDir => {}
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                // Skip RootDir/Prefix here; we'll prepend "/" if the path was absolute
            }
        }
    }
    let mut result = components.join("/");
    if is_absolute {
        result = format!("/{}", result);
    }
    result
}

/// Run one full `Request::Message` turn for a session: stream the provider
/// response, execute any tool calls (with approval gating), append results, and
/// loop until the model stops calling tools. Lives in its own task so the read
/// loop can accept `Request::Cancel` (which aborts us) and
/// `Request::ApprovalResponse` (delivered through `approvals`) concurrently.
#[allow(clippy::too_many_arguments)]
async fn handle_message_turn(
    w: &Socket,
    sessions: SessionMap,
    approvals: &Approvals,
    cfg: &crate::config::Config,
    id: u64,
    session_id_s: String,
    working_dir: String,
    content: String,
    tool_registry: Option<crate::tools::ToolRegistry>,
    interactive: bool,
    provider: Option<String>,
) -> Result<()> {
    // Build the tool definitions to send to the provider. If the active
    // provider/model doesn't support structured tool calling
    // (`supports_tools = false`), we skip tools entirely so the model answers
    // directly instead of emitting broken tool calls that loop.
    let provider_cfg = crate::config::resolve_provider(cfg, provider.as_deref());
    let tool_defs: Vec<crate::tools::ToolDefinition> = if provider_cfg.supports_tools {
        tool_registry
            .as_ref()
            .map(|r| r.all().iter().map(|t| t.to_definition()).collect())
            .unwrap_or_default()
    } else {
        tracing::info!(
            "provider '{}' has supports_tools=false; running without tool calling",
            provider_cfg.name
        );
        Vec::new()
    };
    let tool_defs_ref = if tool_defs.is_empty() {
        None
    } else {
        Some(tool_defs.as_slice())
    };

    let ctx = crate::tools::ToolContext {
        working_dir: std::path::PathBuf::from(&working_dir),
        database_url: cfg.database.url.clone(),
        bash_gate: cfg.server.bash_gate.clone(),
    };

    // Per-session approval cache: tracks file paths the user has already approved
    // across all turns in this session, so repeated edits to the same file don't re-prompt.
    let mut approved_paths: HashSet<String> = {
        let mut map = sessions.write().await;
        if let Some(entry) = map.get_mut(&session_id_s) {
            let paths = std::mem::take(&mut entry.approved_paths);
            info!("loaded approval cache for session {}: {} paths", session_id_s, paths.len());
            paths
        } else {
            info!("no approval cache for session {}", session_id_s);
            HashSet::new()
        }
    };

    /// Extract file paths from an apply_patch patch string.
    /// Returns a list of normalized absolute paths for files being modified.
    let extract_patch_paths = |patch: &str| -> Vec<String> {
        let mut paths = Vec::new();
        for line in patch.lines() {
            if line.starts_with("*** Update File:") || line.starts_with("--- a/") || line.starts_with("+++ b/") {
                if let Some(path_part) = line.split(':').nth(1).or_else(|| line.split('/').nth(1)) {
                    let path = path_part.trim().trim_start_matches("a/").trim_start_matches("b/");
                    if !path.is_empty() {
                        let p = Path::new(path);
                        let absolute = if p.is_relative() {
                            ctx.working_dir.join(p).display().to_string()
                        } else {
                            path.to_string()
                        };
                        let norm = normalize_path_key(&absolute);
                        info!("extract_patch_paths: found path={} absolute={} norm={}", path, absolute, norm);
                        paths.push(norm);
                    }
                }
            }
        }
        paths
    };

    let mut done_sent = false;
    let mut loop_iteration = 0u32;
    const MAX_TOOL_LOOPS: u32 = 20;
    // Loop guard: if the model repeats the exact same tool call (same name +
    // args) several times in a row — usually because it's emitting malformed
    // arguments that keep failing — stop early with a clear message instead of
    // burning through all 20 iterations.
    let mut last_call_sig: Option<String> = None;
    let mut repeat_count = 0u32;
    const MAX_REPEAT: u32 = 3;
    // Also track the total number of tool calls across the whole turn. If the
    // model keeps calling tools without ever producing a final answer, cap it
    // lower than MAX_TOOL_LOOPS so a wandering model (many DIFFERENT calls)
    // doesn't burn the full budget. This catches the "exploring forever"
    // pattern that identical-repeat detection misses.
    let mut total_tool_calls = 0u32;
    const MAX_TOTAL_TOOL_CALLS: u32 = 25;
    // Progress check: track distinct tool calls seen and consecutive errors.
    // If the model keeps hitting tool errors without producing new information,
    // escalate to a clear terminal message instead of letting it spin.
    let mut seen_calls: std::collections::HashSet<String> = HashSet::new();
    let mut consecutive_errors = 0u32;
    const MAX_CONSECUTIVE_ERRORS: u32 = 4;
    // Total APPROVAL_DENIED results this turn. Unlike `consecutive_errors`,
    // this is NOT reset by successful reads/bin reach between denials — the
    // model retrying gated operations over and over (interleaved with reads)
    // should abort quickly instead of burning the whole budget.
    let mut denials_this_turn = 0u32;
    const MAX_DENIALS_PER_TURN: u32 = 3;

    // Empty-response retry: some providers are proxies that route to a
    // different upstream model per request. Occasionally the routed model
    // returns nothing (no text, no tool calls) — often a transient hiccup
    // while the proxy switches models. When that happens we sleep briefly and
    // retry the SAME conversation (the new model reads the full history),
    // resetting the loop-guard counters so it gets a fresh budget. We cap the
    // number of empty-response retries so a genuinely stuck model still gives
    // up instead of looping forever.
    let mut empty_response_retries = 0u32;
    const MAX_EMPTY_RESPONSE_RETRIES: u32 = 3;
    const EMPTY_RESPONSE_RETRY_DELAY_SECS: u64 = 30;

    // Tool-calling loop: send message, execute tools, append results, repeat.
    loop {
        loop_iteration += 1;
        if loop_iteration > MAX_TOOL_LOOPS {
            error!("tool-calling loop exceeded {} iterations, breaking", MAX_TOOL_LOOPS);
            send(w, &Event::Error {
                id: Some(id),
                message: format!("exceeded maximum tool-calling iterations ({})", MAX_TOOL_LOOPS),
            })
            .await?;
            break;
        }

        // Get current conversation for the provider.
        let msgs: Vec<crate::storage::Message> = {
            let s = sessions.read().await;
            s.get(&session_id_s).map(|e| e.messages.clone()).unwrap_or_default()
        };

        let session_model = {
            let s = sessions.read().await;
            s.get(&session_id_s)
                .map(|e| e.model.clone())
                .unwrap_or_else(|| {
                    crate::config::resolve_provider(cfg, provider.as_deref()).default_model.clone()
                })
        };

        // Memory context: durable facts relevant to the working dir are
        // appended to the system prompt automatically.
        let query = if msgs.len() >= 1 { content.clone() } else { String::new() };
        let memory_context = crate::memory::retrieve(&query, &working_dir, 5)
            .into_iter()
            .map(|n| format!("- {}", n.text))
            .collect::<Vec<_>>()
            .join("\n");

        let instructions = crate::agents::load_instructions(&working_dir);

        // Signal the remaining tool budget so the model wraps up instead of
        // exploring forever. MAX_TOTAL_TOOL_CALLS is the hard cap.
        let remaining_budget = if MAX_TOTAL_TOOL_CALLS > total_tool_calls {
            Some((MAX_TOTAL_TOOL_CALLS - total_tool_calls) as u32)
        } else {
            Some(0u32)
        };

        let result = crate::provider::send_message(
            cfg, provider.as_deref(), &session_model, &msgs, tool_defs_ref, &memory_context, &instructions, remaining_budget,
        )
        .await;

        match result {
            Ok(events) => {
                let mut assistant_text = String::new();
                let mut tool_calls: Vec<crate::protocol::ToolCall> = Vec::new();

                for ev in &events {
                    match ev {
                        Event::TextDelta { id: _, text } => {
                            assistant_text.push_str(text);
                            send(w, ev).await?;
                        }
                        Event::ToolCall { id: _, calls } => {
                            for c in calls {
                                send(w, &Event::ToolCall {
                                    id: Some(id),
                                    calls: vec![c.clone()],
                                })
                                .await?;
                            }
                            tool_calls.extend(calls.clone());
                        }
                        Event::Done { id: _ } => {}
                        _ => {
                            send(w, ev).await?;
                        }
                    }
                }

                // Persist assistant text + tool calls, then execute tools.
                {
                    let mut map = sessions.write().await;
                    if let Some(entry) = map.get_mut(&session_id_s) {
                        if !assistant_text.is_empty() || !tool_calls.is_empty() {
                            let stored_tcs: Vec<crate::storage::StoredToolCall> = tool_calls
                                .iter()
                                .map(|tc| crate::storage::StoredToolCall {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    arguments: tc.input.to_string(),
                                })
                                .collect();
                            entry.messages.push(Message {
                                role: "assistant".to_string(),
                                content: assistant_text.clone(),
                                timestamp_ms: chrono::Utc::now().timestamp_millis(),
                                tool_calls: if stored_tcs.is_empty() {
                                    None
                                } else {
                                    Some(stored_tcs)
                                },
                                tool_call_id: None,
                            });
                        }
                        entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                        let snapshot = entry.clone();
                        if let Err(e) = crate::storage::save_session(&snapshot).await {
                            error!("saving session {}: {}", session_id_s, e);
                        }
                    }
                }

                // If no tool calls, done.
                if tool_calls.is_empty() {
                    // If the model produced no text either, it likely hit a
                    // transient proxy/model-switch hiccup (the routed model
                    // returned nothing). Retry the same conversation after a
                    // short delay so the next model can read the full history
                    // and continue — resetting the loop-guard counters so it
                    // gets a fresh budget. Only give up after several retries.
                    if assistant_text.is_empty() && empty_response_retries < MAX_EMPTY_RESPONSE_RETRIES {
                        empty_response_retries += 1;
                        warn!(
                            "model returned empty response (no text, no tool calls); retrying in {}s (attempt {}/{})",
                            EMPTY_RESPONSE_RETRY_DELAY_SECS,
                            empty_response_retries,
                            MAX_EMPTY_RESPONSE_RETRIES
                        );
                        send(w, &Event::TextDelta {
                            id: Some(id),
                            text: format!(
                                "(the model returned no response; retrying with a fresh model in {}s — attempt {}/{})\n",
                                EMPTY_RESPONSE_RETRY_DELAY_SECS,
                                empty_response_retries,
                                MAX_EMPTY_RESPONSE_RETRIES
                            ),
                        })
                        .await?;
                        // Reset the loop-guard counters so the retried model
                        // gets a fresh budget and isn't penalized for the
                        // empty response.
                        loop_iteration = 0;
                        repeat_count = 0;
                        total_tool_calls = 0;
                        consecutive_errors = 0;
                        denials_this_turn = 0;
                        last_call_sig = None;
                        seen_calls.clear();
                        tokio::time::sleep(std::time::Duration::from_secs(EMPTY_RESPONSE_RETRY_DELAY_SECS)).await;
                        continue;
                    }
                    // If the model produced no text either, surface a clear
                    // message instead of appearing to hang.
                    if assistant_text.is_empty() {
                        send(w, &Event::TextDelta {
                            id: Some(id),
                            text: "(the model finished but produced no text response. It may have hit a reasoning/token limit or emitted an unrecognized tool marker.)\n".to_string(),
                        })
                        .await?;
                    }
                    send(w, &Event::Done { id: Some(id) }).await?;
                    done_sent = true;
                    break;
                }

                // Loop guard: detect the model repeating the same tool call
                // (same name + serialized args) over and over. This usually
                // means it's emitting malformed arguments that keep failing
                // (e.g. a reasoning model that doesn't follow the tool schema).
                // Stop early with a clear message instead of looping to the cap.
                let sig = tool_calls
                    .iter()
                    .map(|tc| format!("{}:{}", tc.name, tc.input.to_string()))
                    .collect::<Vec<_>>()
                    .join("|");
                total_tool_calls += tool_calls.len() as u32;
                // Progress check: record distinct calls seen this turn.
                for tc in &tool_calls {
                    seen_calls.insert(format!("{}:{}", tc.name, tc.input.to_string()));
                }
                if last_call_sig.as_deref().map(|s| s == &sig).unwrap_or(false) {
                    repeat_count += 1;
                } else {
                    repeat_count = 0;
                    last_call_sig = Some(sig.clone());
                }
                if repeat_count >= MAX_REPEAT
                    || total_tool_calls >= MAX_TOTAL_TOOL_CALLS
                    || consecutive_errors >= MAX_CONSECUTIVE_ERRORS
                    || denials_this_turn >= MAX_DENIALS_PER_TURN
                {
                    let reason = if denials_this_turn >= MAX_DENIALS_PER_TURN {
                        format!(
                            "{} operations were denied for approval in this turn. The model kept retrying gated operations; stopping.",
                            denials_this_turn
                        )
                    } else if repeat_count >= MAX_REPEAT {
                        format!(
                            "the model kept repeating the same tool call ({}). It may be emitting malformed arguments.",
                            sig.chars().take(80).collect::<String>()
                        )
                    } else if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        format!(
                            "the model hit {} consecutive tool errors without making progress.",
                            consecutive_errors
                        )
                    } else {
                        format!(
                            "the model made {} tool calls without producing a final answer. It may be looping through exploration or retrying denied operations.",
                            total_tool_calls
                        )
                    };
                    error!(
                        "aborting turn: repeat={} total_tool_calls={} distinct={} errors={} sig={}",
                        repeat_count,
                        total_tool_calls,
                        seen_calls.len(),
                        consecutive_errors,
                        sig.chars().take(80).collect::<String>()
                    );
                    send(w, &Event::Error {
                        id: Some(id),
                        message: format!("{} Try a different model or provider, or disable tools.", reason),
                    })
                    .await?;
                    send(w, &Event::Done { id: Some(id) }).await?;
                    done_sent = true;
                    break;
                }

                // Execute each tool and append results as assistant messages.
                // Phase 1: resolve approvals for every tool call (may await
                // user input). Phase 2: execute — independent (read-only)
                // tools run concurrently, dependent tools run sequentially.
                struct PreparedCall<'a> {
                    tc: &'a crate::protocol::ToolCall,
                    tool: Option<&'a dyn crate::tools::Tool>,
                    approved: bool,
                    deny_reason: Option<String>,
                }

                let mut prepared: Vec<PreparedCall> = Vec::new();

                // Helper: resolve a gate path to an absolute, normalized key.
                let resolve_gate_key = |g: &ApprovalGate| -> (String, String) {
                    let path_key = g.path.clone().unwrap_or_default();
                    let resolved_path = if !path_key.is_empty() {
                        let p = Path::new(&path_key);
                        if p.is_relative() {
                            ctx.working_dir.join(p).display().to_string()
                        } else {
                            path_key.clone()
                        }
                    } else {
                        String::new()
                    };
                    let norm_key = normalize_path_key(&resolved_path);
                    (path_key, norm_key)
                };

                // Read-only tools that gate on "outside workspace" reads. When
                // several of these target the same outside directory in one
                // turn, we ask ONE approval for the directory and apply it to
                // all of them, instead of prompting per file.
                let read_only_tools = ["read", "list_dir", "glob", "agentgrep"];

                // Pass 1: for each tool call, decide whether it needs approval
                // and, if it's a read-only outside-workspace read, which
                // directory group it belongs to.
                struct ApprovalPlan<'a> {
                    tc: &'a crate::protocol::ToolCall,
                    tool: Option<&'a dyn crate::tools::Tool>,
                    gate: Option<ApprovalGate>,
                    // For read-only outside reads: the normalized directory key
                    // to batch on (empty = not batchable / needs individual).
                    group_key: String,
                }
                let mut plan: Vec<ApprovalPlan> = Vec::new();
                for tc in &tool_calls {
                    let tool = tool_registry.as_ref().and_then(|r| r.find(&tc.name));
                    let gate = gate_tool(&tc.name, &tc.input, &ctx);
                    let group_key = if read_only_tools.iter().any(|t| t == &tc.name) {
                        if let Some(g) = &gate {
                            let (_, norm) = resolve_gate_key(g);
                            // Group by the DIRECTORY being read, so all reads
                            // inside the same outside directory share one
                            // approval. `list_dir`/`glob` target a directory
                            // already; `read`/`agentgrep` target a file, so
                            // use its parent directory.
                            if !norm.is_empty() {
                                if tc.name == "read" || tc.name == "agentgrep" {
                                    Path::new(&norm).parent().map(|p| p.display().to_string()).unwrap_or(norm.clone())
                                } else {
                                    norm.clone()
                                }
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    plan.push(ApprovalPlan { tc, tool, gate, group_key });
                }

                // Pass 2: resolve approvals. Read-only calls sharing a group
                // key get one prompt for the directory; everything else is
                // prompted individually (or auto-approved).
                let mut group_decisions: HashMap<String, bool> = HashMap::new();
                for ap in &plan {
                    send(w, &Event::Status {
                        id: Some(id),
                        message: format!("Executing tool: {}...", ap.tc.name),
                    })
                    .await?;
                    let (approved, deny_reason) = match &ap.gate {
                        None => (true, None),
                        Some(g) => match cfg.server.approve_mode.as_str() {
                            "auto" => (true, None),
                            "deny" => (false, Some(g.reason.clone())),
                            _ => {
                                if interactive {
                                    let (path_key, norm_key) = resolve_gate_key(g);
                                    info!("approval check: tool={} path_key={} norm_key={} cache_size={} cache_contains={}",
                                        ap.tc.name, path_key, norm_key, approved_paths.len(), approved_paths.contains(&norm_key));
                                    // Already approved this exact path this session.
                                    if !norm_key.is_empty() && approved_paths.contains(&norm_key) {
                                        info!("auto-approving {} (already approved in this session): {}", ap.tc.name, g.reason);
                                        (true, None)
                                    } else if !ap.group_key.is_empty() && group_decisions.contains_key(&ap.group_key) {
                                        // Another read-only call already prompted for this directory.
                                        let ok = group_decisions.get(&ap.group_key).map(|b| *b).unwrap_or(false);
                                        if ok && !norm_key.is_empty() {
                                            approved_paths.insert(norm_key);
                                        }
                                        (ok, if ok { None } else { Some(g.reason.clone()) })
                                    } else {
                                        // Prompt once. For a read-only group,
                                        // list the files being read so the user
                                        // knows what they're approving; otherwise
                                        // describe the individual tool.
                                        let prompt_path = if !ap.group_key.is_empty() {
                                            Some(ap.group_key.clone())
                                        } else {
                                            g.path.clone()
                                        };
                                        let prompt_reason = if !ap.group_key.is_empty() {
                                            // Collect all file paths in this
                                            // directory group for the prompt.
                                            let files = plan
                                                .iter()
                                                .filter(|p| p.group_key == ap.group_key)
                                                .map(|p| {
                                                    let (_, n) = resolve_gate_key(p.gate.as_ref().unwrap());
                                                    n
                                                })
                                                .filter(|n| !n.is_empty())
                                                .collect::<Vec<_>>();
                                            if files.len() > 1 {
                                                format!(
                                                    "read {} files outside workspace in {}:\n  {}",
                                                    files.len(),
                                                    ap.group_key,
                                                    files.join("\n  ")
                                                )
                                            } else {
                                                format!("read outside workspace ({})", ap.group_key)
                                            }
                                        } else {
                                            g.reason.clone()
                                        };
                                        send(w, &Event::ApprovalRequired {
                                            id,
                                            tool_call_id: ap.tc.id.clone(),
                                            tool_name: ap.tc.name.clone(),
                                            path: prompt_path,
                                            reason: prompt_reason,
                                        })
                                        .await?;
                                        info!("asking approval for {} ({})", ap.tc.name, g.reason);
                                        let key = format!("{}:{}", id, ap.tc.id);
                                        let (tx, rx) = tokio::sync::oneshot::channel();
                                        approvals.write().await.insert(key.clone(), tx);
                                        let decision = tokio::time::timeout(
                                            std::time::Duration::from_secs(300),
                                            rx,
                                        )
                                        .await;
                                        approvals.write().await.remove(&key);
                                        let ok = matches!(decision, Ok(Ok(true)));
                                        // Cache the group decision so sibling
                                        // read-only calls in the same directory
                                        // don't re-prompt.
                                        if !ap.group_key.is_empty() {
                                            group_decisions.insert(ap.group_key.clone(), ok);
                                        }
                                        if ok && !norm_key.is_empty() {
                                            approved_paths.insert(norm_key.clone());
                                            info!("inserted into approval cache: norm_key={} cache_size={}", norm_key, approved_paths.len());
                                        }
                                        // Also extract paths from apply_patch patches to populate cache
                                        if ok && ap.tc.name == "apply_patch" {
                                            info!("apply_patch approved, extracting paths from patch");
                                            if let Some(patch) = ap.tc.input.get("patch").and_then(|v| v.as_str()) {
                                                let extracted = extract_patch_paths(patch);
                                                info!("extract_patch_paths returned: {:?}", extracted);
                                                for p in extracted {
                                                    if !p.is_empty() {
                                                        approved_paths.insert(p.clone());
                                                        info!("inserted patch path into cache: {}", p);
                                                    }
                                                }
                                            } else {
                                                info!("apply_patch has no patch field in input");
                                            }
                                        }
                                        (ok, if ok { None } else { Some(g.reason.clone()) })
                                    }
                                } else {
                                    info!("auto-approving {} in non-interactive mode: {}", ap.tc.name, g.reason);
                                    (true, None)
                                }
                            }
                        }
                    };
                    prepared.push(PreparedCall { tc: ap.tc, tool: ap.tool, approved, deny_reason });
                }

                // Phase 2a: run all approved, independent (read-only) tools
                // concurrently, collecting results keyed by tool_call_id.
                let mut independent_ids: Vec<String> = Vec::new();
                let mut independent_futures: Vec<
                    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>,
                > = Vec::new();
                for pc in &prepared {
                    if pc.approved {
                        if let Some(tool) = pc.tool {
                            if tool.is_independent() {
                                independent_ids.push(pc.tc.id.clone());
                                let input = pc.tc.input.clone();
                                let ctx_owned = ctx.clone();
                                independent_futures.push(Box::pin(async move {
                                    tool.execute(&input, &ctx_owned).await
                                }));
                            }
                        }
                    }
                }
                let joined = futures::future::join_all(independent_futures).await;
                let mut independent_results: HashMap<String, Result<String>> = HashMap::new();
                for (id, res) in independent_ids.into_iter().zip(joined) {
                    independent_results.insert(id, res);
                }

                // Phase 2b: assemble results in original order. Independent
                // calls use the concurrent results; dependent / denied /
                // missing tools run (or resolve) sequentially.
                let mut results: Vec<crate::protocol::ToolResultEntry> = Vec::new();
                for pc in &prepared {
                    let result_entry = if !pc.approved {
                        warn!("tool denied: {} ({})", pc.tc.name, pc.deny_reason.as_deref().unwrap_or("no approval"));
                        denials_this_turn += 1;
                        crate::protocol::ToolResultEntry {
                            tool_call_id: pc.tc.id.clone(),
                            output: format!("APPROVAL_DENIED: {}", pc.deny_reason.as_deref().unwrap_or("no approval")),
                            is_error: true,
                        }
                    } else if let Some(tool) = pc.tool {
                        if tool.is_independent() {
                            match independent_results.remove(&pc.tc.id) {
                                Some(Ok(s)) => crate::protocol::ToolResultEntry {
                                    tool_call_id: pc.tc.id.clone(),
                                    output: s,
                                    is_error: false,
                                },
                                Some(Err(e)) => crate::protocol::ToolResultEntry {
                                    tool_call_id: pc.tc.id.clone(),
                                    output: format!("ERROR: {}", e),
                                    is_error: true,
                                },
                                None => crate::protocol::ToolResultEntry {
                                    tool_call_id: pc.tc.id.clone(),
                                    output: "ERROR: independent tool produced no result".to_string(),
                                    is_error: true,
                                },
                            }
                        } else {
                            match tool.execute(&pc.tc.input, &ctx).await {
                                Ok(s) => crate::protocol::ToolResultEntry {
                                    tool_call_id: pc.tc.id.clone(),
                                    output: s,
                                    is_error: false,
                                },
                                Err(e) => crate::protocol::ToolResultEntry {
                                    tool_call_id: pc.tc.id.clone(),
                                    output: format!("ERROR: {}", e),
                                    is_error: true,
                                },
                            }
                        }
                    } else {
                        crate::protocol::ToolResultEntry {
                            tool_call_id: pc.tc.id.clone(),
                            output: format!("ERROR: tool '{}' not found", pc.tc.name),
                            is_error: true,
                        }
                    };
                    send(w, &Event::ToolResult {
                        id: Some(id),
                        results: vec![result_entry.clone()],
                    })
                    .await?;
                    results.push(result_entry);
                }

                // Progress check: count consecutive tool errors. If every tool
                // in this batch errored, bump the counter; otherwise reset it.
                if !results.is_empty() && results.iter().all(|r| r.is_error) {
                    consecutive_errors += 1;
                } else {
                    consecutive_errors = 0;
                }

                // Append tool results to session conversation.
                {
                    let mut map = sessions.write().await;
                    if let Some(entry) = map.get_mut(&session_id_s) {
                        for r in &results {
                            entry.messages.push(Message {
                                role: "tool".to_string(),
                                content: r.output.clone(),
                                timestamp_ms: chrono::Utc::now().timestamp_millis(),
                                tool_calls: None,
                                tool_call_id: Some(r.tool_call_id.clone()),
                            });
                        }
                        entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                        let snapshot = entry.clone();
                        if let Err(e) = crate::storage::save_session(&snapshot).await {
                            error!("saving session {}: {}", session_id_s, e);
                        }
                    }
                }

                // Loop continues — next iteration sends the tool results to provider.
            }
            Err(e) => {
                eprintln!("provider error: {}", e);
                tracing::error!("provider call failed: {}", e);
                send(w, &Event::Error {
                    id: Some(id),
                    message: e.to_string(),
                })
                .await?;
                break;
            }
        }
    }

    if !done_sent {
        send(w, &Event::Done { id: Some(id) }).await?;
    }

    // Save approval cache back to session for next turn
    {
        let mut map = sessions.write().await;
        if let Some(entry) = map.get_mut(&session_id_s) {
            info!("saving approval cache for session {}: {} paths", session_id_s, approved_paths.len());
            entry.approved_paths = approved_paths;
        }
    }

    Ok(())
}

/// Append the assistant turn for this session (if any text was produced) and
/// persist it. Idempotent: safe to call multiple times per turn.
async fn persist_assistant(sessions: &SessionMap, session_id: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut map = sessions.write().await;
    if let Some(entry) = map.get_mut(session_id) {
        entry.messages.push(Message {
            role: "assistant".to_string(),
            content: text.to_string(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            tool_calls: None,
            tool_call_id: None,
        });
        entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
        let snapshot = entry.clone();
        drop(map);
        if let Err(e) = crate::storage::save_session(&snapshot).await {
            error!("saving session {}: {}", session_id, e);
        } else {
            tracing::info!("saved session {} ({} messages)", session_id, snapshot.messages.len());
        }
    } else {
        tracing::warn!("session {} not found in map during persist", session_id);
    }
}

async fn send(w: &Socket, ev: &Event) -> Result<()> {
    let data = serde_json::to_string(ev)?;
    let mut w = w.lock().await;
    w.write_all(data.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

/// Push a `Notification` event to a session's connected interactive client, if
/// any. Uses a shared socket handle (`try_write`, which needs only `&self`) so
/// a swarm handler on another connection (spawn / DM / broadcast) can surface
/// the event live in that client's chat.
async fn notify_session(
    session_clients: &SessionClients,
    target_session: &str,
    from_session: Option<&str>,
    notification_type: crate::protocol::NotificationType,
    message: String,
) {
    let ev = crate::swarm::interrupt_event(from_session, notification_type, message);
    let Ok(data) = serde_json::to_string(&ev).map(|d| d + "\n") else {
        return;
    };
    let sock = {
        let map = session_clients.read().await;
        map.get(target_session).cloned()
    };
    let Some(sock) = sock else { return };

    let payload = data.as_bytes();
    let mut written = 0;
    loop {
        match sock.try_write(&payload[written..]) {
            Ok(n) => {
                written += n;
                if written >= payload.len() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if sock.writable().await.is_err() {
                    // Dead socket — drop the registration.
                    session_clients.write().await.remove(target_session);
                    return;
                }
            }
            Err(_) => {
                // Dead socket — drop the registration.
                session_clients.write().await.remove(target_session);
                return;
            }
        }
    }
}

/// Run a spawned headless agent session to completion: feed its conversation
/// through the provider, executing any tool calls (auto-approved — no human is
/// attached) and appending results, until a turn has no tool calls or the loop
/// budget is exhausted. Returns the accumulated assistant text for the report.
async fn run_headless_agent(
    cfg: &crate::config::Config,
    sessions: &SessionMap,
    session_id: &str,
    provider: Option<&str>,
) -> Result<String> {
    let tool_registry = {
        let mut reg = crate::tools::default_registry();
        for t in crate::mcp::load_tools(cfg).await {
            reg.register_boxed(t);
        }
        reg
    };
    let tool_defs: Vec<crate::tools::ToolDefinition> =
        tool_registry.all().iter().map(|t| t.to_definition()).collect();
    let tool_defs_ref = if tool_defs.is_empty() { None } else { Some(tool_defs.as_slice()) };

    let working_dir = {
        let s = sessions.read().await;
        s.get(session_id)
            .map(|e| e.working_dir.clone())
            .unwrap_or_default()
    };
    let ctx = crate::tools::ToolContext {
        working_dir: std::path::PathBuf::from(&working_dir),
        database_url: cfg.database.url.clone(),
        bash_gate: cfg.server.bash_gate.clone(),
    };

    let instructions = crate::agents::load_instructions(&working_dir);

    const MAX_TOOL_LOOPS: u32 = 20;
    let mut total_text = String::new();
    let mut iteration = 0u32;
    // Empty-response retry (same rationale as the interactive path): a proxy
    // provider may route to a model that returns nothing; retry the same
    // conversation after a short delay so the next model can continue.
    let mut empty_response_retries = 0u32;
    const MAX_EMPTY_RESPONSE_RETRIES: u32 = 3;
    const EMPTY_RESPONSE_RETRY_DELAY_SECS: u64 = 30;

    loop {
        iteration += 1;
        if iteration > MAX_TOOL_LOOPS {
            info!("headless agent {} exceeded {} tool loops", session_id, MAX_TOOL_LOOPS);
            total_text.push_str(&format!(
                "\n[stopped: exceeded {} tool-calling iterations]",
                MAX_TOOL_LOOPS
            ));
            break;
        }

        let msgs: Vec<crate::storage::Message> = {
            let s = sessions.read().await;
            s.get(session_id).map(|e| e.messages.clone()).unwrap_or_default()
        };

        let session_model = {
            let s = sessions.read().await;
            s.get(session_id)
                .map(|e| e.model.clone())
                .unwrap_or_else(|| {
                    crate::config::resolve_provider(cfg, provider.as_deref()).default_model.clone()
                })
        };

        let events = match crate::provider::send_message(cfg, provider, &session_model, &msgs, tool_defs_ref, "", &instructions, None).await {
            Ok(events) => events,
            Err(e) => {
                let msg = format!("ERROR: {}", e);
                total_text.push_str(&msg);
                persist_assistant(sessions, session_id, &msg).await;
                return Ok(total_text);
            }
        };

        let mut turn_text = String::new();
        let mut tool_calls: Vec<crate::protocol::ToolCall> = Vec::new();
        for ev in &events {
            if let Event::TextDelta { id: _, text } = ev {
                turn_text.push_str(text);
            }
            if let Event::ToolCall { id: _, calls } = ev {
                tool_calls.extend(calls.clone());
            }
        }
        total_text.push_str(&turn_text);

        // Persist the assistant turn (text + tool calls).
        {
            let mut map = sessions.write().await;
            if let Some(entry) = map.get_mut(session_id) {
                if !turn_text.is_empty() || !tool_calls.is_empty() {
                    let stored_tcs: Vec<crate::storage::StoredToolCall> = tool_calls
                        .iter()
                        .map(|tc| crate::storage::StoredToolCall {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments: tc.input.to_string(),
                        })
                        .collect();
                    entry.messages.push(Message {
                        role: "assistant".to_string(),
                        content: turn_text.clone(),
                        timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        tool_calls: if stored_tcs.is_empty() { None } else { Some(stored_tcs) },
                        tool_call_id: None,
                    });
                }
                entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                let snapshot = entry.clone();
                if let Err(e) = crate::storage::save_session(&snapshot).await {
                    error!("saving session {}: {}", session_id, e);
                }
            }
        }

        if tool_calls.is_empty() {
            // If the model produced no text either, it likely hit a transient
            // proxy/model-switch hiccup. Retry the same conversation after a
            // short delay so the next model can read the history and continue.
            if turn_text.is_empty() && empty_response_retries < MAX_EMPTY_RESPONSE_RETRIES {
                empty_response_retries += 1;
                info!(
                    "headless agent {}: model returned empty response; retrying in {}s (attempt {}/{})",
                    session_id,
                    EMPTY_RESPONSE_RETRY_DELAY_SECS,
                    empty_response_retries,
                    MAX_EMPTY_RESPONSE_RETRIES
                );
                iteration = 0; // reset loop budget for the retried model
                tokio::time::sleep(std::time::Duration::from_secs(EMPTY_RESPONSE_RETRY_DELAY_SECS)).await;
                continue;
            }
            break;
        }

        // Execute each tool (auto-approved: headless agent, no human attached).
        let mut results: Vec<crate::protocol::ToolResultEntry> = Vec::new();
        for tc in &tool_calls {
            let result_entry = match tool_registry.find(&tc.name) {
                Some(tool) => match tool.execute(&tc.input, &ctx).await {
                    Ok(s) => crate::protocol::ToolResultEntry {
                        tool_call_id: tc.id.clone(),
                        output: s,
                        is_error: false,
                    },
                    Err(e) => crate::protocol::ToolResultEntry {
                        tool_call_id: tc.id.clone(),
                        output: format!("ERROR: {}", e),
                        is_error: true,
                    },
                },
                None => crate::protocol::ToolResultEntry {
                    tool_call_id: tc.id.clone(),
                    output: format!("ERROR: tool '{}' not found", tc.name),
                    is_error: true,
                },
            };
            results.push(result_entry);
        }

        {
            let mut map = sessions.write().await;
            if let Some(entry) = map.get_mut(session_id) {
                for r in &results {
                    entry.messages.push(Message {
                        role: "tool".to_string(),
                        content: r.output.clone(),
                        timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        tool_calls: None,
                        tool_call_id: Some(r.tool_call_id.clone()),
                    });
                }
                entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                let snapshot = entry.clone();
                if let Err(e) = crate::storage::save_session(&snapshot).await {
                    error!("saving session {}: {}", session_id, e);
                }
            }
        }
    }

    Ok(total_text)
}

/// Spawn a new headless agent session and run its initial message.
async fn handle_swarm_spawn(
    w: &Socket,
    req_id: u64,
    sessions: &SessionMap,
    swarm: &SharedSwarm,
    session_clients: &SessionClients,
    parent_session_id: Option<String>,
    initial_message: &str,
    model_override: Option<&str>,
    provider_override: Option<&str>,
    label: Option<&str>,
) -> Result<()> {
    let cfg = crate::config::load()?;
    let new_id = crate::protocol::new_message_id();
    let session_id = format!("agent-{}", new_id);

    {
        let mut map = sessions.write().await;
        let working_dir = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let model = model_override
            .unwrap_or(&crate::config::resolve_provider(&cfg, provider_override).default_model)
            .to_string();
        let entry = map.entry(session_id.clone()).or_insert_with(|| Session {
            id: session_id.clone(),
            title: format!("agent-{}", new_id),
            working_dir,
            model,
            messages: Vec::new(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            approved_paths: HashSet::new(),
        });
        entry.messages.push(Message {
            role: "user".to_string(),
            content: initial_message.to_string(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            tool_calls: None,
            tool_call_id: None,
        });
        entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
    }

    {
        let mut sw = swarm.write().await;
        let member = swarm::make_member(
            &session_id,
            label.map(String::from),
            parent_session_id.clone(),
            true,
        );
        sw.register_member(member);
        sw.update_status(&session_id, "running", Some(initial_message.to_string()));
    }

    send(w, &Event::Spawned {
        id: req_id,
        new_session_id: session_id.clone(),
        label: label.map(String::from),
    })
    .await?;

    // Run the agent headlessly with the full tool set (auto-approved — no
    // human is attached). Returns the accumulated assistant text.
    let assistant_text = run_headless_agent(&cfg, sessions, &session_id, provider_override).await?;

    // Forward the completion report to the parent session (jancode's
    // report_back_to_session_id policy): queue a soft interrupt / send a
    // Notification to the parent's connected clients if any.
    let report = if assistant_text.starts_with("ERROR: ") {
        format!("Agent {} failed: {}", session_id, assistant_text)
    } else {
        format!("Agent {} completed: {}", session_id, assistant_text)
    };

    {
        let mut sw = swarm.write().await;
        sw.update_status(&session_id, "completed", None);
        if let Some(parent_id) = &parent_session_id {
            sw.queue_interrupt(
                parent_id,
                Interrupt {
                    from_session: Some(session_id.clone()),
                    notification_type: crate::protocol::NotificationType::CompletionReport,
                    message: report.clone(),
                },
            );
        }
    }

    // Push the completion report live to the parent's interactive client (if
    // one is attached), independent of the swarm interrupt queue.
    if let Some(parent_id) = &parent_session_id {
        notify_session(
            session_clients,
            parent_id,
            Some(&session_id),
            crate::protocol::NotificationType::CompletionReport,
            report.clone(),
        )
        .await;
    }

    Ok(())
}

/// Deliver a direct message to another session as a soft interrupt.
async fn handle_swarm_dm(
    w: &Socket,
    swarm: &SharedSwarm,
    session_clients: &SessionClients,
    from_session_id: Option<&str>,
    to_session_id: &str,
    message: &str,
) -> Result<()> {
    {
        let mut sw = swarm.write().await;
        sw.queue_interrupt(
            to_session_id,
            Interrupt {
                from_session: from_session_id.map(String::from),
                notification_type: crate::protocol::NotificationType::Dm,
                message: message.to_string(),
            },
        );
    }
    notify_session(
        session_clients,
        to_session_id,
        from_session_id,
        crate::protocol::NotificationType::Dm,
        message.to_string(),
    )
    .await;
    send(
        w,
        &swarm::interrupt_event(
            from_session_id,
            crate::protocol::NotificationType::Dm,
            format!("delivered DM to {}", to_session_id),
        ),
    )
    .await?;
    Ok(())
}

/// Broadcast a message to every member of the swarm.
async fn handle_swarm_broadcast(
    _w: &Socket,
    swarm: &SharedSwarm,
    session_clients: &SessionClients,
    from_session_id: Option<&str>,
    message: &str,
) -> Result<()> {
    let keys: Vec<String> = {
        let sw = swarm.read().await;
        sw.members.keys().cloned().collect()
    };
    {
        let mut sw = swarm.write().await;
        for to in &keys {
            sw.queue_interrupt(
                to,
                Interrupt {
                    from_session: from_session_id.map(String::from),
                    notification_type: crate::protocol::NotificationType::Broadcast,
                    message: message.to_string(),
                },
            );
        }
    }
    for to in &keys {
        notify_session(
            session_clients,
            to,
            from_session_id,
            crate::protocol::NotificationType::Broadcast,
            message.to_string(),
        )
        .await;
    }
    info!(
        "broadcast from {:?} to all members: {}",
        from_session_id, message
    );
    Ok(())
}

/// Stop a session. `force` is required to stop a session outside the caller's
/// spawn subtree (mirrors jancode's stop permissions).
async fn handle_swarm_stop(
    w: &Socket,
    swarm: &SharedSwarm,
    id: u64,
    session_id: &str,
    force: bool,
) -> Result<()> {
    let ok = {
        let sw = swarm.read().await;
        match sw.get_member(session_id) {
            None => false,
            Some(m) => force || m.parent_session_id.is_none(),
        }
    };
    if !ok && !force {
        send(
            w,
            &Event::Error {
                id: Some(id),
                message: format!(
                    "not authorized to stop {} (outside your subtree; use --force)",
                    session_id
                ),
            },
        )
        .await?;
        return Ok(());
    }
    {
        let mut sw = swarm.write().await;
        sw.remove_member(session_id);
        sw.update_status(session_id, "stopped", None);
    }
    send(w, &Event::Stopped { id, session_id: session_id.to_string() }).await?;
    Ok(())

}

/// Return the live status of one member, or the requesting session itself.
async fn handle_swarm_status(
    w: &Socket,
    swarm: &SharedSwarm,
    id: u64,
    session_id: Option<&str>,
) -> Result<()> {
    let sw = swarm.read().await;
    let target = session_id
        .or_else(|| sw.members.keys().next().map(|s| s.as_str()));
    if let Some(sid) = target {
        if let Some(m) = sw.get_member(sid) {
            send(
                w,
                &Event::MemberStatus {
                    id,
                    session_id: sid.to_string(),
                    status: m.status.clone(),
                    detail: m.detail.clone(),
                    task_label: m.task_label.clone(),
                },
            )
            .await?;
            return Ok(());
        }
    }
    send(
        w,
        &Event::Error {
            id: Some(id),
            message: "session not found".to_string(),
        },
    )
    .await?;
    Ok(())
}

/// Return the full member roster.
async fn handle_swarm_list(
    w: &Socket,
    swarm: &SharedSwarm,
    id: u64,
) -> Result<()> {
    let sw = swarm.read().await;
    send(w, &Event::MemberList { id, members: sw.member_list() }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_logic() {
        let ctx = ToolContext { working_dir: std::path::PathBuf::from("/work/proj"), database_url: String::new(), bash_gate: "basic".to_string() };
        // write/edit/apply_patch always gate.
        assert!(gate_tool("write", &serde_json::json!({"path": "x.rs"}), &ctx).is_some());
        assert!(gate_tool("edit", &serde_json::json!({"path": "x.rs"}), &ctx).is_some());
        assert!(gate_tool("apply_patch", &serde_json::json!({}), &ctx).is_some());
        // In-workspace reads are ungated.
        assert!(gate_tool("read", &serde_json::json!({"path": "x.rs"}), &ctx).is_none());
        assert!(gate_tool("agentgrep", &serde_json::json!({"query": "foo"}), &ctx).is_none());
        // Reads outside the workspace are gated.
        assert!(gate_tool("read", &serde_json::json!({"path": "../secret"}), &ctx).is_some());
        assert!(gate_tool("read", &serde_json::json!({"path": "/etc/passwd"}), &ctx).is_some());
        // Glob with an escaping base is gated.
        assert!(gate_tool("glob", &serde_json::json!({"pattern": "**", "path": "../"}), &ctx).is_some());
        // bash: in-workspace commands are ungated, escapes are gated.
        assert!(gate_tool("bash", &serde_json::json!({"command": "ls"}), &ctx).is_none());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cargo test"}), &ctx).is_none());
        assert!(gate_tool("bash", &serde_json::json!({"command": "grep -r alpha src"}), &ctx).is_none());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cat /etc/hosts"}), &ctx).is_some());
        assert!(gate_tool("bash", &serde_json::json!({"command": "ls ~/sysadmin-mcp"}), &ctx).is_some());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cd .. && ls"}), &ctx).is_some());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cd /tmp && pwd"}), &ctx).is_some());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cat $HOME/.claude/settings.json"}), &ctx).is_some());
        assert!(gate_tool("plan", &serde_json::json!({"action": "show"}), &ctx).is_none());
        // Read-only git actions are ungated.
        assert!(gate_tool("git", &serde_json::json!({"action": "status"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "log"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "diff"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "remote", "branch": "x"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "fetch"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "stash", "stash_action": "list"}), &ctx).is_none());
        // Creating a branch pointer at HEAD is ungated.
        assert!(gate_tool("git", &serde_json::json!({"action": "branch", "branch": "topic"}), &ctx).is_none());
        // Mutating git actions are gated.
        assert!(gate_tool("git", &serde_json::json!({"action": "add", "path": "all"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "commit", "message": "x"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "push"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "pull"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "checkout", "branch": "x"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "branch", "branch": "old", "delete_branch": true}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "stash", "stash_action": "pop"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "merge", "branch": "main"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "rebase", "branch": "origin/main"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "reset", "mode": "hard", "ref": "HEAD~1"}), &ctx).is_some());
        // Web + notes.
        assert!(gate_tool("fetch_url", &serde_json::json!({"url": "https://example.com"}), &ctx).is_none());
        assert!(gate_tool("note", &serde_json::json!({"action": "clear"}), &ctx).is_none());
        // http_request: read methods ungated, mutating gated.
        assert!(gate_tool("http_request", &serde_json::json!({"method": "GET", "url": "https://a.com"}), &ctx).is_none());
        assert!(gate_tool("http_request", &serde_json::json!({"method": "HEAD", "url": "https://a.com"}), &ctx).is_none());
        assert!(gate_tool("http_request", &serde_json::json!({"method": "post", "url": "https://a.com", "json": {}}), &ctx).is_some());
        assert!(gate_tool("http_request", &serde_json::json!({"method": "DELETE", "url": "https://a.com/x"}), &ctx).is_some());
        // docker: reads ungated, state changes gated.
        assert!(gate_tool("docker", &serde_json::json!({"action": "ps"}), &ctx).is_none());
        assert!(gate_tool("docker", &serde_json::json!({"action": "logs", "container": "web"}), &ctx).is_none());
        assert!(gate_tool("docker", &serde_json::json!({"action": "inspect", "container": "web"}), &ctx).is_none());
        assert!(gate_tool("docker", &serde_json::json!({"action": "exec", "container": "web", "command": "ls"}), &ctx).is_some());
        assert!(gate_tool("docker", &serde_json::json!({"action": "run", "image": "nginx"}), &ctx).is_some());
        assert!(gate_tool("docker", &serde_json::json!({"action": "rm", "container": "web", "force": true}), &ctx).is_some());
        assert!(gate_tool("docker", &serde_json::json!({"action": "build", "tag": "x"}), &ctx).is_some());
        // sql: reads ungated, writes gated.
        assert!(gate_tool("sql", &serde_json::json!({"query": "SELECT 1"}), &ctx).is_none());
        assert!(gate_tool("sql", &serde_json::json!({"query": "show tables"}), &ctx).is_none());
        assert!(gate_tool("sql", &serde_json::json!({"query": "INSERT INTO t VALUES (1)"}), &ctx).is_some());
        assert!(gate_tool("sql", &serde_json::json!({"query": "DELETE FROM t"}), &ctx).is_some());
    }
}
