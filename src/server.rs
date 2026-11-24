use crate::config::runtime_dir;
use crate::protocol::{Event, Request};
use crate::storage::{Message, Session};
use crate::swarm::{self, Interrupt, SwarmState};
use crate::tools::{is_outside, patch_paths, ToolContext};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700));
    }
    let socket_path = runtime.join("jancode.sock");
    if socket_path.exists() {
        // Never unlink a socket another live daemon is listening on: that
        // would steal its name and leave the old daemon on an unlinked inode.
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            anyhow::bail!(
                "jancode daemon already running at {}",
                socket_path.display()
            );
        }
        std::fs::remove_file(&socket_path).context("removing stale socket")?;
    }
    let listener = tokio::net::UnixListener::bind(&socket_path).context("binding unix socket")?;
    // Owner-only socket: the protocol has no authentication, so file
    // permissions are the only thing stopping other local users from driving
    // the agent or reading session history.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
    }
    info!("jancode daemon listening on {}", socket_path.display());

    let sessions: SessionMap = Arc::new(RwLock::new(HashMap::new()));
    for s in crate::storage::list_sessions().await? {
        sessions.write().await.insert(s.id.clone(), s);
    }

    // Reconstruct the swarm roster from persisted sessions so spawned agents
    // survive daemon restarts (jancode reloads swarm state from disk).
    let swarm: SharedSwarm = Arc::new(RwLock::new(SwarmState::new()));
    {
        let mut sw = swarm.write().await;
        let live = sessions.read().await;
        for s in live.values() {
            // Members are persisted on the session. Legacy sessions created
            // before that field existed are recovered from their title, but
            // without a parent link (so they remain stoppable).
            let legacy_agent = s.title.starts_with("agent-");
            if !s.is_swarm_member && !legacy_agent {
                continue;
            }
            let is_headless = s.is_headless || legacy_agent;
            let member = swarm::make_member(&s.id, None, s.parent_session_id.clone(), is_headless);
            sw.register_member(member);
            if is_headless {
                sw.update_status(&s.id, "idle", None);
            } else {
                sw.update_status(&s.id, "active", None);
            }
        }
    }

    // Idle shutdown: like jancode, the daemon exits on its own once no client is
    // connected long enough, so `run`/`connect` re-spawn it lazily instead of
    // leaving a permanent background process.
    let session_clients: SessionClients = Arc::new(RwLock::new(HashMap::new()));
    let active_clients: ActiveClients = Arc::new(AtomicUsize::new(0));
    let idle_secs = cfg.server.idle_timeout_secs;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    if idle_secs > 0 {
        let watcher = active_clients.clone();
        // Don't shut down while a detached swarm agent is still running: the
        // spawn CLI exits immediately, so connected clients alone would make
        // the daemon look idle and kill the in-flight agent.
        let swarm_watch = swarm.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(idle_secs)).await;
                let no_clients = watcher.load(Ordering::SeqCst) == 0;
                let no_agents = swarm_watch.read().await.tasks.is_empty();
                if no_clients && no_agents {
                    info!("idle timeout reached ({}s), shutting down", idle_secs);
                    shutdown.notify_one();
                    return;
                }
                if !no_agents {
                    tracing::debug!("idle timeout pending: swarm agents still running");
                }
            }
        });
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
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
            _ = shutdown.notified() => {
                info!("idle timeout reached, shutting down");
                break;
            }
        }
    }
    Ok(())
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
            Request::GetHistory { id, session_id } => {
                // Honor `session_id`: returning every session's messages would
                // leak other workspaces' transcripts to any connected client.
                let msgs: Vec<crate::protocol::Message> = match session_id {
                    Some(sid) => {
                        let s = sessions.read().await;
                        s.get(&sid)
                            .map(|entry| {
                                entry
                                    .messages
                                    .iter()
                                    .map(|m| crate::protocol::Message {
                                        role: m.role.clone(),
                                        content: m.content.clone(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    }
                    None => Vec::new(),
                };
                send(&w, &Event::History { id, messages: msgs }).await?;
            }
            Request::Compact { id, session_id } => {
                let sid = match session_id {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        send(
                            &w,
                            &Event::Error {
                                id: Some(id),
                                message: "compact: session_id required".to_string(),
                            },
                        )
                        .await?;
                        continue;
                    }
                };
                let (model, messages) = {
                    let map = sessions.read().await;
                    match map.get(&sid) {
                        Some(entry) => (entry.model.clone(), entry.messages.clone()),
                        None => {
                            send(
                                &w,
                                &Event::Error {
                                    id: Some(id),
                                    message: format!("compact: no session {}", sid),
                                },
                            )
                            .await?;
                            continue;
                        }
                    }
                };
                match compact_messages(&cfg, None, &model, &messages, cfg.server.compact_keep).await
                {
                    Ok((new_messages, removed, kept)) => {
                        let snapshot = {
                            let mut map = sessions.write().await;
                            match map.get_mut(&sid) {
                                Some(entry) => {
                                    entry.messages = new_messages;
                                    entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                                    Some(entry.clone())
                                }
                                None => None,
                            }
                        };
                        if let Some(snap) = snapshot {
                            let _ = crate::storage::save_session(&snap).await;
                        }
                        send(&w, &Event::Compacted { id, removed, kept }).await?;
                    }
                    Err(e) => {
                        send(
                            &w,
                            &Event::Error {
                                id: Some(id),
                                message: format!("compact failed: {}", e),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::Message {
                id,
                session_id,
                content,
                tools,
                mcp,
                model,
                provider,
                cwd,
                interactive,
                plan_mode,
                images,
            } => {
                let session_id_s = session_id.unwrap_or_else(|| format!("session-{}", id));
                let working_dir = cwd.clone().unwrap_or_else(|| {
                    std::env::current_dir()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
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
                            sw.update_status(&session_id_s, "active", None);
                        }
                    }
                    // Persist membership so the roster survives a restart.
                    let snapshot = {
                        let mut map = sessions.write().await;
                        match map.get_mut(&session_id_s) {
                            Some(entry) => {
                                entry.is_swarm_member = true;
                                entry.is_headless = false;
                                Some(entry.clone())
                            }
                            None => None,
                        }
                    };
                    if let Some(snap) = snapshot {
                        let _ = crate::storage::save_session(&snap).await;
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
                    // Honor the client's requested names: the allow-list is part
                    // of the protocol, not decoration.
                    if let Some(names) = &tools {
                        reg.retain_names(names);
                    }
                    // MCP tools are opt-in per turn (client sets `mcp: true` for
                    // `/mcp <message>`). Ordinary turns skip them entirely so a
                    // slow/down MCP server can't add latency to every message.
                    if mcp {
                        let mcp_cfg = reload_cfg(&cfg);
                        for t in crate::mcp::load_tools(&mcp_cfg).await {
                            reg.register_boxed(t);
                        }
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
                            crate::config::resolve_provider(cfg.as_ref(), provider.as_deref())
                                .default_model
                                .clone()
                        }),
                        messages: Vec::new(),
                        created_at_ms: chrono::Utc::now().timestamp_millis(),
                        updated_at_ms: chrono::Utc::now().timestamp_millis(),
                        approved_paths: HashSet::new(),
                        is_swarm_member: false,
                        parent_session_id: None,
                        is_headless: false,
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
                let _ = crate::memory::auto_capture(&content, &working_dir).await;

                send(&w, &Event::Ack { id }).await?;
                send(
                    &w,
                    &Event::Status {
                        id: Some(id),
                        message: "Thinking...".to_string(),
                    },
                )
                .await?;

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
                    if let Err(e) = handle_message_turn(
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
                        plan_mode,
                        images,
                        provider,
                    )
                    .await
                    {
                        let _ = send(
                            &turn_w,
                            &Event::Error {
                                id: Some(id),
                                message: e.to_string(),
                            },
                        )
                        .await;
                        let _ = send(&turn_w, &Event::Done { id: Some(id) }).await;
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
            Request::ApprovalResponse {
                id,
                tool_call_id,
                approved,
                ..
            } => {
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
                    &w,
                    id,
                    &sessions,
                    &swarm,
                    &session_clients,
                    parent_session_id,
                    &initial_message,
                    model.as_deref(),
                    provider.as_deref(),
                    label.as_deref(),
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
                    &w,
                    &swarm,
                    &session_clients,
                    from_session_id.as_deref(),
                    &to_session_id,
                    &message,
                )
                .await?;
            }
            Request::SwarmBroadcast {
                id: _,
                from_session_id,
                message,
            } => {
                handle_swarm_broadcast(
                    &w,
                    &swarm,
                    &session_clients,
                    from_session_id.as_deref(),
                    &message,
                )
                .await?;
            }
            Request::SwarmStop {
                id,
                session_id,
                force,
            } => {
                handle_swarm_stop(&w, &sessions, &swarm, id, &session_id, force).await?;
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
            let method = input
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();
            if matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
                None
            } else {
                Some(ApprovalGate {
                    path: None,
                    reason: format!(
                        "HTTP {} to {}",
                        method,
                        input.get("url").and_then(|v| v.as_str()).unwrap_or("<url>")
                    ),
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
            if !crate::tools::sql_is_read_only(query) {
                let preview: String = query.chars().take(60).collect();
                return Some(ApprovalGate {
                    path: None,
                    reason: format!("run SQL: {}", preview),
                });
            }
            // A read-only query can still read an arbitrary sqlite file; gate
            // when it resolves outside the workspace.
            let db = input.get("db").and_then(|v| v.as_str()).unwrap_or("");
            let is_url = db.starts_with("postgres://")
                || db.starts_with("postgresql://")
                || db.starts_with("mysql://");
            if !db.is_empty() && !is_url {
                let path = db.strip_prefix("sqlite:").unwrap_or(db);
                let full = ctx.resolve_path(path);
                if is_outside(&ctx.working_dir, &full) {
                    return Some(ApprovalGate {
                        path: Some(full.display().to_string()),
                        reason: format!("read database outside workspace ({})", full.display()),
                    });
                }
            }
            None
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
        "apply_patch" => {
            // Show the files the patch touches so the approval is meaningful
            // (and so a `../../` escape is visible before the tool runs).
            let paths = patch_paths(input.get("patch").and_then(|v| v.as_str()).unwrap_or(""));
            let reason = if paths.is_empty() {
                "apply a multi-file patch".to_string()
            } else {
                format!("apply a multi-file patch ({})", paths.join(", "))
            };
            Some(ApprovalGate { path: None, reason })
        }
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
            crate::tools::bash_escapes_workspace(command, &ctx.working_dir, &ctx.bash_gate).map(
                |reason| ApprovalGate {
                    path: None,
                    reason: format!("bash may touch outside workspace: {}", reason),
                },
            )
        }
        "jq" => {
            // Read-only, but the file can resolve outside the workspace.
            let p = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if p.is_empty() {
                None
            } else {
                let full = ctx.resolve_path(&p);
                if is_outside(&ctx.working_dir, &full) {
                    Some(ApprovalGate {
                        path: Some(full.display().to_string()),
                        reason: format!("read JSON outside workspace ({})", full.display()),
                    })
                } else {
                    None
                }
            }
        }
        "gh" => {
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if crate::tools::gh_is_mutating(action) {
                Some(ApprovalGate {
                    path: None,
                    reason: format!("gh {}", action),
                })
            } else {
                None
            }
        }
        "test" => {
            // Known test runners are read-only-ish dev actions (like
            // `cargo test` through bash); a custom command can do anything.
            let runner = input.get("runner").and_then(|v| v.as_str()).unwrap_or("");
            if runner == "custom" {
                Some(ApprovalGate {
                    path: None,
                    reason: "run a custom test command".to_string(),
                })
            } else {
                None
            }
        }
        "sandbox" => {
            // `exec` stays inside an already-hardened, network-isolated
            // container; creating, feeding, extracting, or removing a sandbox
            // is gated.
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(action, "list" | "exec") {
                None
            } else {
                Some(ApprovalGate {
                    path: None,
                    reason: format!("sandbox {}", action),
                })
            }
        }
        "ssh" => {
            // Remote code execution: always gated in interactive mode.
            let host = input.get("host").and_then(|v| v.as_str()).unwrap_or("");
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let preview: String = command.chars().take(60).collect();
            Some(ApprovalGate {
                path: None,
                reason: format!("ssh {}: {}", host, preview),
            })
        }
        "ssh_copy" => {
            // Writes to a remote host (copy_to) or the workspace (copy_from).
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let host = input.get("host").and_then(|v| v.as_str()).unwrap_or("");
            let remote_path = input
                .get("remote_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(ApprovalGate {
                path: None,
                reason: format!("scp {} {}:{}", action, host, remote_path),
            })
        }
        "download" => {
            // Network read, but it writes a file into the workspace.
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let full = ctx.resolve_path(p);
            Some(ApprovalGate {
                path: Some(full.display().to_string()),
                reason: format!(
                    "download {} to {}",
                    input.get("url").and_then(|v| v.as_str()).unwrap_or("<url>"),
                    full.display()
                ),
            })
        }
        "jenkins" => {
            // GET actions (job_info/console/artifacts) are read-only; only the
            // artifact download writes to the workspace.
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if action == "artifact" {
                let p = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let full = ctx.resolve_path(p);
                Some(ApprovalGate {
                    path: Some(full.display().to_string()),
                    reason: format!("download jenkins artifact to {}", full.display()),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// True when `[security]` allow rules cover this call (so it shouldn't prompt).
fn policy_allows(name: &str, input: &Value, sec: &crate::config::SecurityConfig) -> bool {
    if sec.allow_tools.iter().any(|t| t == name) {
        return true;
    }
    if name == "bash" {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            for pat in &sec.allow_bash_patterns {
                if let Ok(re) = regex::Regex::new(pat) {
                    if re.is_match(cmd) {
                        return true;
                    }
                }
            }
        }
    }
    if matches!(name, "ssh" | "ssh_copy") {
        if let Some(host) = input.get("host").and_then(|v| v.as_str()) {
            if sec.allow_hosts.iter().any(|h| h == host) {
                return true;
            }
        }
    }
    false
}

/// Minimal standard base64 encoder (no external dependency).
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Read local image files and encode them as data URLs for multimodal input.
async fn load_image_data(working_dir: &str, paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let full = if std::path::Path::new(p).is_absolute() {
            std::path::PathBuf::from(p)
        } else {
            std::path::Path::new(working_dir).join(p)
        };
        let bytes = tokio::fs::read(&full)
            .await
            .with_context(|| format!("reading image {}", full.display()))?;
        if bytes.len() > 15 * 1024 * 1024 {
            anyhow::bail!("image {} is larger than 15 MB", full.display());
        }
        let mime = match full
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            _ => "application/octet-stream",
        };
        out.push(format!("data:{};base64,{}", mime, base64_encode(&bytes)));
    }
    Ok(out)
}

/// Rough token estimate for a message list (~4 chars/token).
fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let content = m.content.len() as u64;
            let tools = m.tool_calls.as_ref().map_or(0u64, |tcs| {
                tcs.iter().map(|tc| tc.arguments.len() as u64).sum()
            });
            (content + tools) / 4
        })
        .sum()
}

/// Summarize the older part of a session with the provider and return a new
/// message list: a `[compacted context]` user message plus the kept tail.
async fn compact_messages(
    cfg: &crate::config::Config,
    provider_name: Option<&str>,
    model: &str,
    messages: &[Message],
    keep: usize,
) -> Result<(Vec<Message>, usize, usize)> {
    if messages.len() <= keep + 2 {
        anyhow::bail!("nothing to compact");
    }
    let max_cut = messages.len().saturating_sub(keep);
    // Prefer a user message without tool calls as the boundary so the kept tail
    // never starts with an orphaned tool result.
    let mut cut = max_cut;
    for i in (1..=max_cut).rev() {
        if messages[i].role == "user" && messages[i].tool_calls.is_none() {
            cut = i;
            break;
        }
    }
    if cut == 0 || cut >= messages.len() {
        anyhow::bail!("nothing to compact");
    }
    let kept = &messages[cut..];

    let mut summary_input: Vec<Message> = messages[..cut].to_vec();
    summary_input.push(Message {
        role: "user".to_string(),
        content: "Summarize the conversation above concisely. Preserve decisions, file paths, commands, errors, and pending tasks. Output only the summary.".to_string(),
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        tool_calls: None,
        tool_call_id: None,
    });

    let events = crate::provider::send_message(
        cfg,
        provider_name,
        model,
        &summary_input,
        None,
        "",
        "",
        None,
        &[],
    )
    .await?;

    let mut summary = String::new();
    for ev in events {
        if let crate::protocol::Event::TextDelta { text, .. } = ev {
            summary.push_str(&text);
        }
    }
    let summary = summary.trim().to_string();
    if summary.is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }

    let mut new_messages = Vec::with_capacity(kept.len() + 1);
    new_messages.push(Message {
        role: "user".to_string(),
        content: format!("[compacted context]\n{}", summary),
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        tool_calls: None,
        tool_call_id: None,
    });
    new_messages.extend_from_slice(kept);
    Ok((new_messages, cut, kept.len()))
}

/// Normalize a path string for use as a cache key.
/// Handles variations like "index.html", "./index.html", "dir/../index.html"
/// by resolving to a canonical form without touching the filesystem.
fn normalize_path_key(path: &str) -> String {
    use std::path::Path;
    let is_absolute = Path::new(path).is_absolute();
    let mut components = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            std::path::Component::Normal(name) => {
                components.push(name.to_string_lossy().into_owned())
            }
            std::path::Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
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
    plan_mode: bool,
    images: Vec<String>,
    provider: Option<String>,
) -> Result<()> {
    let image_data = load_image_data(&working_dir, &images).await?;

    // Auto-compact once per turn when the estimated context is near the
    // provider's window. `/compact` does the same on demand.
    if cfg.server.compact_threshold > 0.0 {
        let compaction_model = {
            let map = sessions.read().await;
            map.get(&session_id_s)
                .map(|e| e.model.clone())
                .unwrap_or_default()
        };
        let provider_cfg = crate::config::resolve_provider(cfg, provider.as_deref());
        if provider_cfg.context_window > 0 && !compaction_model.is_empty() {
            let current = {
                let map = sessions.read().await;
                map.get(&session_id_s)
                    .map(|e| e.messages.clone())
                    .unwrap_or_default()
            };
            let est = estimate_messages_tokens(&current);
            let threshold =
                (provider_cfg.context_window as f64 * cfg.server.compact_threshold) as u64;
            if est > threshold {
                info!(
                    "auto-compacting session {}: est {} > threshold {}",
                    session_id_s, est, threshold
                );
                match compact_messages(
                    cfg,
                    provider.as_deref(),
                    &compaction_model,
                    &current,
                    cfg.server.compact_keep,
                )
                .await
                {
                    Ok((new_messages, removed, _)) => {
                        let snapshot = {
                            let mut map = sessions.write().await;
                            match map.get_mut(&session_id_s) {
                                Some(entry) => {
                                    entry.messages = new_messages;
                                    entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                                    Some(entry.clone())
                                }
                                None => None,
                            }
                        };
                        if let Some(snap) = snapshot {
                            let _ = crate::storage::save_session(&snap).await;
                        }
                        let _ = send(
                            w,
                            &Event::Status {
                                id: Some(id),
                                message: format!("compacted {} older messages", removed),
                            },
                        )
                        .await;
                    }
                    Err(e) => warn!("auto-compact failed: {}", e),
                }
            }
        }
    }

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

    let ctx = crate::tools::ToolContext::new(
        std::path::PathBuf::from(&working_dir),
        cfg.database.url.clone(),
        cfg.server.bash_gate.clone(),
        cfg.tools.allowed_private_hosts.clone(),
    );

    // Per-session approval cache: tracks file paths the user has already approved
    // across all turns in this session, so repeated edits to the same file don't re-prompt.
    let mut approved_paths: HashSet<String> = {
        // Clone, don't `mem::take`: a cancelled or panicked turn must not wipe
        // the persisted cache.
        let map = sessions.read().await;
        let paths = map
            .get(&session_id_s)
            .map(|entry| entry.approved_paths.clone())
            .unwrap_or_default();
        info!(
            "loaded approval cache for session {}: {} paths",
            session_id_s,
            paths.len()
        );
        paths
    };

    // Extract file paths from an apply_patch patch string.
    // Returns a list of normalized absolute paths for files being modified.
    let extract_patch_paths = |patch: &str| -> Vec<String> {
        let mut paths = Vec::new();
        for line in patch.lines() {
            if line.starts_with("*** Update File:")
                || line.starts_with("--- a/")
                || line.starts_with("+++ b/")
            {
                if let Some(path_part) = line.split(':').nth(1).or_else(|| line.split('/').nth(1)) {
                    let path = path_part
                        .trim()
                        .trim_start_matches("a/")
                        .trim_start_matches("b/");
                    if !path.is_empty() {
                        let p = Path::new(path);
                        let absolute = if p.is_relative() {
                            ctx.working_dir.join(p).display().to_string()
                        } else {
                            path.to_string()
                        };
                        let norm = normalize_path_key(&absolute);
                        info!(
                            "extract_patch_paths: found path={} absolute={} norm={}",
                            path, absolute, norm
                        );
                        paths.push(norm);
                    }
                }
            }
        }
        paths
    };

    let mut done_sent = false;
    let mut loop_iteration = 0u32;
    // Configurable so big projects can explore longer. Defaults: 50 iterations
    // / 75 tool calls (see `[server] max_tool_loops` / `max_total_tool_calls`).
    let max_tool_loops = cfg.server.max_tool_loops.max(1);
    // Loop guard: if the model repeats the exact same tool call (same name +
    // args) several times in a row — usually because it's emitting malformed
    // arguments that keep failing — stop early with a clear message instead of
    // burning through the whole iteration budget.
    let mut last_call_sig: Option<String> = None;
    let mut repeat_count = 0u32;
    const MAX_REPEAT: u32 = 3;
    // Also track the total number of tool calls across the whole turn. If the
    // model keeps calling tools without ever producing a final answer, cap it
    // so a wandering model (many DIFFERENT calls) doesn't burn the full budget.
    // This catches the "exploring forever" pattern that identical-repeat
    // detection misses.
    let mut total_tool_calls = 0u32;
    let max_total_tool_calls = cfg.server.max_total_tool_calls.max(1);
    // Progress check: track distinct tool calls seen and consecutive errors.
    // If the model keeps hitting tool errors without producing new information,
    // escalate to a clear terminal message instead of letting it spin.
    // `seen_calls` maps call-signature -> how many times it was issued this
    // turn, so we can catch a model that keeps re-issuing the *same* call
    // (e.g. re-reading one file) even when interleaved with other calls.
    let mut seen_calls: std::collections::HashMap<String, u32> = HashMap::new();
    const MAX_SAME_CALL_REPEATS: u32 = 5;
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
        if loop_iteration > max_tool_loops {
            error!(
                "tool-calling loop exceeded {} iterations, breaking",
                max_tool_loops
            );
            send(
                w,
                &Event::Error {
                    id: Some(id),
                    message: format!(
                        "exceeded maximum tool-calling iterations ({})",
                        max_tool_loops
                    ),
                },
            )
            .await?;
            break;
        }

        // Get current conversation for the provider.
        let msgs: Vec<crate::storage::Message> = {
            let s = sessions.read().await;
            s.get(&session_id_s)
                .map(|e| e.messages.clone())
                .unwrap_or_default()
        };

        let session_model = {
            let s = sessions.read().await;
            s.get(&session_id_s)
                .map(|e| e.model.clone())
                .unwrap_or_else(|| {
                    crate::config::resolve_provider(cfg, provider.as_deref())
                        .default_model
                        .clone()
                })
        };

        // Memory context: durable facts relevant to the working dir are
        // appended to the system prompt automatically.
        let query = if !msgs.is_empty() {
            content.clone()
        } else {
            String::new()
        };
        let memory_context = crate::memory::retrieve(&query, &working_dir, 5)
            .await
            .into_iter()
            .map(|n| format!("- {}", n.text))
            .collect::<Vec<_>>()
            .join("\n");

        let mut instructions = crate::agents::load_instructions(&working_dir);
        if plan_mode {
            instructions.push_str(
                "\n\n# Plan mode (active)\nRead-only investigation is allowed, but every mutating operation is denied automatically. Investigate and produce a concrete plan; do not attempt writes, edits, state-changing commands, or remote changes.\n",
            );
        }

        // Signal the remaining tool budget so the model wraps up instead of
        // exploring forever. max_total_tool_calls is the hard cap.
        let remaining_budget = if max_total_tool_calls > total_tool_calls {
            Some(max_total_tool_calls - total_tool_calls)
        } else {
            Some(0u32)
        };

        let result = crate::provider::send_message(
            cfg,
            provider.as_deref(),
            &session_model,
            &msgs,
            tool_defs_ref,
            &memory_context,
            &instructions,
            remaining_budget,
            &image_data,
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
                                send(
                                    w,
                                    &Event::ToolCall {
                                        id: Some(id),
                                        calls: vec![c.clone()],
                                    },
                                )
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
                    if assistant_text.is_empty()
                        && empty_response_retries < MAX_EMPTY_RESPONSE_RETRIES
                    {
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
                        tokio::time::sleep(std::time::Duration::from_secs(
                            EMPTY_RESPONSE_RETRY_DELAY_SECS,
                        ))
                        .await;
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
                    .map(|tc| format!("{}:{}", tc.name, tc.input))
                    .collect::<Vec<_>>()
                    .join("|");
                total_tool_calls += tool_calls.len() as u32;
                // Progress check: count how many times each distinct call was
                // issued this turn. Repeating the *same* call many times (even
                // interleaved with other calls) means the model is stuck — e.g.
                // re-reading the same file over and over.
                let mut max_same_call = 0u32;
                for tc in &tool_calls {
                    let c = seen_calls
                        .entry(format!("{}:{}", tc.name, tc.input))
                        .or_insert(0);
                    *c += 1;
                    max_same_call = max_same_call.max(*c);
                }
                if last_call_sig.as_deref().map(|s| s == sig).unwrap_or(false) {
                    repeat_count += 1;
                } else {
                    repeat_count = 0;
                    last_call_sig = Some(sig.clone());
                }
                if repeat_count >= MAX_REPEAT
                    || total_tool_calls >= max_total_tool_calls
                    || consecutive_errors >= MAX_CONSECUTIVE_ERRORS
                    || denials_this_turn >= MAX_DENIALS_PER_TURN
                    || max_same_call >= MAX_SAME_CALL_REPEATS
                {
                    let reason = if max_same_call >= MAX_SAME_CALL_REPEATS {
                        format!(
                            "the model issued the same tool call {} times without progress (e.g. re-reading the same file). Stopping to avoid a loop.",
                            max_same_call
                        )
                    } else if denials_this_turn >= MAX_DENIALS_PER_TURN {
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
                            "the model made {} tool calls without producing a final answer. It may be looping through exploration or retrying denied operations. Increase `[server] max_tool_loops` / `max_total_tool_calls` for very large projects.",
                            total_tool_calls
                        )
                    };
                    error!(
                        "aborting turn: repeat={} total_tool_calls={} distinct={} max_same={} errors={} sig={}",
                        repeat_count,
                        total_tool_calls,
                        seen_calls.len(),
                        max_same_call,
                        consecutive_errors,
                        sig.chars().take(80).collect::<String>()
                    );
                    send(
                        w,
                        &Event::Error {
                            id: Some(id),
                            message: format!(
                                "{} Try a different model or provider, or disable tools.",
                                reason
                            ),
                        },
                    )
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
                    let raw_gate = gate_tool(&tc.name, &tc.input, &ctx);
                    let gate = if !plan_mode && policy_allows(&tc.name, &tc.input, &cfg.security) {
                        None
                    } else {
                        raw_gate
                    };
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
                                    Path::new(&norm)
                                        .parent()
                                        .map(|p| p.display().to_string())
                                        .unwrap_or(norm.clone())
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
                    plan.push(ApprovalPlan {
                        tc,
                        tool,
                        gate,
                        group_key,
                    });
                }

                // Pass 2: resolve approvals. Read-only calls sharing a group
                // key get one prompt for the directory; everything else is
                // prompted individually (or auto-approved).
                let mut group_decisions: HashMap<String, bool> = HashMap::new();
                for ap in &plan {
                    send(
                        w,
                        &Event::Status {
                            id: Some(id),
                            message: format!("Executing tool: {}...", ap.tc.name),
                        },
                    )
                    .await?;
                    let force_deny = cfg.security.deny_tools.iter().any(|t| t == &ap.tc.name);
                    let (approved, deny_reason) = if force_deny {
                        (
                            false,
                            Some(format!(
                                "'{}' is denied by [security] deny_tools",
                                ap.tc.name
                            )),
                        )
                    } else if plan_mode {
                        match &ap.gate {
                            None => (true, None),
                            Some(_) => (
                                false,
                                Some("plan mode: mutating operations are disabled".to_string()),
                            ),
                        }
                    } else {
                        match &ap.gate {
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
                                        if !norm_key.is_empty()
                                            && approved_paths.contains(&norm_key)
                                        {
                                            info!("auto-approving {} (already approved in this session): {}", ap.tc.name, g.reason);
                                            (true, None)
                                        } else if !ap.group_key.is_empty()
                                            && group_decisions.contains_key(&ap.group_key)
                                        {
                                            // Another read-only call already prompted for this directory.
                                            let ok = group_decisions
                                                .get(&ap.group_key)
                                                .copied()
                                                .unwrap_or(false);
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
                                                        let (_, n) = resolve_gate_key(
                                                            p.gate.as_ref().unwrap(),
                                                        );
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
                                                    format!(
                                                        "read outside workspace ({})",
                                                        ap.group_key
                                                    )
                                                }
                                            } else {
                                                g.reason.clone()
                                            };
                                            send(
                                                w,
                                                &Event::ApprovalRequired {
                                                    id,
                                                    tool_call_id: ap.tc.id.clone(),
                                                    tool_name: ap.tc.name.clone(),
                                                    path: prompt_path,
                                                    reason: prompt_reason,
                                                },
                                            )
                                            .await?;
                                            info!(
                                                "asking approval for {} ({})",
                                                ap.tc.name, g.reason
                                            );
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
                                                info!(
                                                "apply_patch approved, extracting paths from patch"
                                            );
                                                if let Some(patch) = ap
                                                    .tc
                                                    .input
                                                    .get("patch")
                                                    .and_then(|v| v.as_str())
                                                {
                                                    let extracted = extract_patch_paths(patch);
                                                    info!(
                                                        "extract_patch_paths returned: {:?}",
                                                        extracted
                                                    );
                                                    for p in extracted {
                                                        if !p.is_empty() {
                                                            approved_paths.insert(p.clone());
                                                            info!(
                                                            "inserted patch path into cache: {}",
                                                            p
                                                        );
                                                        }
                                                    }
                                                } else {
                                                    info!(
                                                        "apply_patch has no patch field in input"
                                                    );
                                                }
                                            }
                                            (ok, if ok { None } else { Some(g.reason.clone()) })
                                        }
                                    } else {
                                        info!(
                                            "auto-approving {} in non-interactive mode: {}",
                                            ap.tc.name, g.reason
                                        );
                                        (true, None)
                                    }
                                }
                            },
                        }
                    };
                    // If the user approved an outside-workspace write, record it
                    // so the tool's confinement check allows exactly that path.
                    // Headless runs never populate this, so their writes stay
                    // confined to the workspace.
                    if approved && interactive {
                        if let Some(g) = &ap.gate {
                            let (_, norm_key) = resolve_gate_key(g);
                            if !norm_key.is_empty() {
                                crate::tools::allow_outside(&ctx, &norm_key);
                            }
                        }
                        if ap.tc.name == "apply_patch" {
                            if let Some(patch) = ap.tc.input.get("patch").and_then(|v| v.as_str()) {
                                for p in crate::tools::patch_paths(patch) {
                                    crate::tools::allow_outside(&ctx, &p);
                                }
                            }
                        }
                    }
                    prepared.push(PreparedCall {
                        tc: ap.tc,
                        tool: ap.tool,
                        approved,
                        deny_reason,
                    });
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
                        warn!(
                            "tool denied: {} ({})",
                            pc.tc.name,
                            pc.deny_reason.as_deref().unwrap_or("no approval")
                        );
                        denials_this_turn += 1;
                        crate::protocol::ToolResultEntry {
                            tool_call_id: pc.tc.id.clone(),
                            output: format!(
                                "APPROVAL_DENIED: {}",
                                pc.deny_reason.as_deref().unwrap_or("no approval")
                            ),
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
                                    output: "ERROR: independent tool produced no result"
                                        .to_string(),
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
                    send(
                        w,
                        &Event::ToolResult {
                            id: Some(id),
                            results: vec![result_entry.clone()],
                        },
                    )
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
                send(
                    w,
                    &Event::Error {
                        id: Some(id),
                        message: e.to_string(),
                    },
                )
                .await?;
                break;
            }
        }
    }

    if !done_sent {
        send(w, &Event::Done { id: Some(id) }).await?;
    }

    // Save approval cache back to the session (memory + disk) for next turn.
    {
        let snapshot = {
            let mut map = sessions.write().await;
            match map.get_mut(&session_id_s) {
                Some(entry) => {
                    info!(
                        "saving approval cache for session {}: {} paths",
                        session_id_s,
                        approved_paths.len()
                    );
                    entry.approved_paths = approved_paths;
                    Some(entry.clone())
                }
                None => None,
            }
        };
        if let Some(snap) = snapshot {
            if let Err(e) = crate::storage::save_session(&snap).await {
                error!("saving approval cache for {}: {}", session_id_s, e);
            }
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
            tracing::info!(
                "saved session {} ({} messages)",
                session_id,
                snapshot.messages.len()
            );
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

/// Queue a soft interrupt only when the target has no live client. When a
/// client is connected, `notify_session` delivers the event live, so queueing
/// it too would duplicate the message when the queue is later drained.
async fn queue_interrupt_offline(
    swarm: &SharedSwarm,
    session_clients: &SessionClients,
    target_session: &str,
    interrupt: Interrupt,
) {
    let online = session_clients.read().await.contains_key(target_session);
    if !online {
        swarm
            .write()
            .await
            .queue_interrupt(target_session, interrupt);
    }
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
    swarm: &SharedSwarm,
    session_id: &str,
    provider: Option<&str>,
) -> Result<String> {
    let provider_cfg = crate::config::resolve_provider(cfg, provider);
    // Built-in tools only: headless agents have no way to opt in to MCP, and
    // connecting to MCP servers per turn would add latency (and stall on a down
    // server). Interactive `/mcp <message>` is the MCP entry point.
    let tool_registry = crate::tools::default_registry();
    let tool_defs: Vec<crate::tools::ToolDefinition> = if provider_cfg.supports_tools {
        tool_registry
            .all()
            .iter()
            .map(|t| t.to_definition())
            .collect()
    } else {
        tracing::info!(
            "provider '{}' has supports_tools=false; headless agent runs without tools",
            provider_cfg.name
        );
        Vec::new()
    };
    let tool_defs_ref = if tool_defs.is_empty() {
        None
    } else {
        Some(tool_defs.as_slice())
    };

    let working_dir = {
        let s = sessions.read().await;
        s.get(session_id)
            .map(|e| e.working_dir.clone())
            .unwrap_or_default()
    };
    let ctx = crate::tools::ToolContext::new(
        std::path::PathBuf::from(&working_dir),
        cfg.database.url.clone(),
        cfg.server.bash_gate.clone(),
        cfg.tools.allowed_private_hosts.clone(),
    );

    let instructions = crate::agents::load_instructions(&working_dir);

    // Configurable so big projects can explore longer (see `[server] max_tool_loops`).
    let max_tool_loops = cfg.server.max_tool_loops.max(1);
    let max_total_tool_calls = cfg.server.max_total_tool_calls.max(1);
    const MAX_SAME_CALL_REPEATS: u32 = 5;
    const MAX_CONSECUTIVE_ERRORS: u32 = 4;
    let mut total_tool_calls = 0u32;
    let mut seen_calls: HashMap<String, u32> = HashMap::new();
    let mut consecutive_errors = 0u32;
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
        if iteration > max_tool_loops {
            info!(
                "headless agent {} exceeded {} tool loops",
                session_id, max_tool_loops
            );
            total_text.push_str(&format!(
                "\n[stopped: exceeded {} tool-calling iterations]",
                max_tool_loops
            ));
            break;
        }

        // Deliver soft-interrupt notifications that queued while this agent was
        // offline (DMs / broadcasts), then clear the queue.
        {
            let pending = swarm.write().await.drain_interrupts(session_id);
            if !pending.is_empty() {
                let mut map = sessions.write().await;
                if let Some(entry) = map.get_mut(session_id) {
                    for interrupt in pending {
                        let from = interrupt.from_session.as_deref().unwrap_or("system");
                        entry.messages.push(Message {
                            role: "user".to_string(),
                            content: format!(
                                "[{}] from {}: {}",
                                format!("{:?}", interrupt.notification_type).to_lowercase(),
                                from,
                                interrupt.message
                            ),
                            timestamp_ms: chrono::Utc::now().timestamp_millis(),
                            tool_calls: None,
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
        }

        let msgs: Vec<crate::storage::Message> = {
            let s = sessions.read().await;
            s.get(session_id)
                .map(|e| e.messages.clone())
                .unwrap_or_default()
        };

        let session_model = {
            let s = sessions.read().await;
            s.get(session_id)
                .map(|e| e.model.clone())
                .unwrap_or_else(|| {
                    crate::config::resolve_provider(cfg, provider)
                        .default_model
                        .clone()
                })
        };

        let events = match crate::provider::send_message(
            cfg,
            provider,
            &session_model,
            &msgs,
            tool_defs_ref,
            "",
            &instructions,
            None,
            &[],
        )
        .await
        {
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
                // Reset the loop-guard counters so the retried model gets a
                // fresh budget.
                iteration = 0;
                total_tool_calls = 0;
                seen_calls.clear();
                consecutive_errors = 0;
                tokio::time::sleep(std::time::Duration::from_secs(
                    EMPTY_RESPONSE_RETRY_DELAY_SECS,
                ))
                .await;
                continue;
            }
            break;
        }

        // Loop guards, mirroring the interactive path so a wandering or stuck
        // model can't burn the whole budget.
        total_tool_calls += tool_calls.len() as u32;
        let mut max_same_call = 0u32;
        for tc in &tool_calls {
            let c = seen_calls
                .entry(format!("{}:{}", tc.name, tc.input))
                .or_insert(0);
            *c += 1;
            max_same_call = max_same_call.max(*c);
        }
        if total_tool_calls >= max_total_tool_calls
            || max_same_call >= MAX_SAME_CALL_REPEATS
            || consecutive_errors >= MAX_CONSECUTIVE_ERRORS
        {
            let reason = if max_same_call >= MAX_SAME_CALL_REPEATS {
                format!(
                    "the model issued the same tool call {} times without progress",
                    max_same_call
                )
            } else if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                format!(
                    "the model hit {} consecutive tool errors without progress",
                    consecutive_errors
                )
            } else {
                format!(
                    "the model made {} tool calls without producing a final answer",
                    total_tool_calls
                )
            };
            error!("headless agent {} stopping: {}", session_id, reason);
            total_text.push_str(&format!("\n[stopped: {}]", reason));
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

        if !results.is_empty() && results.iter().all(|r| r.is_error) {
            consecutive_errors += 1;
        } else {
            consecutive_errors = 0;
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
#[allow(clippy::too_many_arguments)]
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

    // Inherit the parent's workspace: the daemon's own cwd is whatever
    // directory it was first launched from (or the service's
    // WorkingDirectory), which is wrong for an agent spawned from another
    // project.
    let working_dir = match &parent_session_id {
        Some(parent_id) => {
            let parent_dir = {
                let map = sessions.read().await;
                map.get(parent_id).map(|s| s.working_dir.clone())
            };
            match parent_dir {
                Some(d) if !d.is_empty() => d,
                _ => std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            }
        }
        None => std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
    };

    {
        let mut map = sessions.write().await;
        let model = model_override
            .unwrap_or(&crate::config::resolve_provider(&cfg, provider_override).default_model)
            .to_string();
        let entry = map.entry(session_id.clone()).or_insert_with(|| Session {
            id: session_id.clone(),
            title: format!("agent-{}", new_id),
            working_dir: working_dir.clone(),
            model,
            messages: Vec::new(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            approved_paths: HashSet::new(),
            is_swarm_member: true,
            parent_session_id: parent_session_id.clone(),
            is_headless: true,
        });
        // Keep the fields correct even if the session already existed.
        entry.working_dir = working_dir;
        entry.is_swarm_member = true;
        entry.parent_session_id = parent_session_id.clone();
        entry.is_headless = true;
        entry.messages.push(Message {
            role: "user".to_string(),
            content: initial_message.to_string(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            tool_calls: None,
            tool_call_id: None,
        });
        entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
        let snapshot = entry.clone();
        if let Err(e) = crate::storage::save_session(&snapshot).await {
            error!("saving spawned session {}: {}", session_id, e);
        }
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

    send(
        w,
        &Event::Spawned {
            id: req_id,
            new_session_id: session_id.clone(),
            label: label.map(String::from),
        },
    )
    .await?;

    // Run the agent in its own task (auto-approved — no human is attached) so
    // the spawning connection isn't blocked for the agent's whole lifetime and
    // `swarm stop` can abort it.
    let task_sessions = sessions.clone();
    let task_swarm = swarm.clone();
    let task_clients = session_clients.clone();
    let task_session_id = session_id.clone();
    let task_parent = parent_session_id.clone();
    let task_provider = provider_override.map(|s| s.to_string());
    let handle = tokio::spawn(async move {
        let result = run_headless_agent(
            &cfg,
            &task_sessions,
            &task_swarm,
            &task_session_id,
            task_provider.as_deref(),
        )
        .await;
        let (status, report) = match result {
            Ok(text) if text.starts_with("ERROR: ") => (
                "failed",
                format!("Agent {} failed: {}", task_session_id, text),
            ),
            Ok(text) => (
                "completed",
                format!("Agent {} completed: {}", task_session_id, text),
            ),
            Err(e) => ("failed", format!("Agent {} failed: {}", task_session_id, e)),
        };
        {
            let mut sw = task_swarm.write().await;
            // Keep the completion/failure report on the member so
            // `swarm status --session <id>` shows it even when the agent had
            // no parent to notify.
            sw.update_status(&task_session_id, status, Some(report.clone()));
            sw.tasks.remove(&task_session_id);
        }
        if let Some(parent_id) = &task_parent {
            // Queue only if the parent is offline; a connected client gets the
            // live notification below.
            queue_interrupt_offline(
                &task_swarm,
                &task_clients,
                parent_id,
                Interrupt {
                    from_session: Some(task_session_id.clone()),
                    notification_type: crate::protocol::NotificationType::CompletionReport,
                    message: report.clone(),
                },
            )
            .await;
            notify_session(
                &task_clients,
                parent_id,
                Some(&task_session_id),
                crate::protocol::NotificationType::CompletionReport,
                report,
            )
            .await;
        }
    });
    swarm.write().await.tasks.insert(session_id, handle);

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
    queue_interrupt_offline(
        swarm,
        session_clients,
        to_session_id,
        Interrupt {
            from_session: from_session_id.map(String::from),
            notification_type: crate::protocol::NotificationType::Dm,
            message: message.to_string(),
        },
    )
    .await;
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
    for to in &keys {
        queue_interrupt_offline(
            swarm,
            session_clients,
            to,
            Interrupt {
                from_session: from_session_id.map(String::from),
                notification_type: crate::protocol::NotificationType::Broadcast,
                message: message.to_string(),
            },
        )
        .await;
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
    sessions: &SessionMap,
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
        sw.update_status(session_id, "stopped", None);
        // Abort a running headless agent; a cooperative stop can't interrupt a
        // long-running tool call.
        if let Some(handle) = sw.tasks.remove(session_id) {
            handle.abort();
        }
        sw.remove_member(session_id);
    }
    // Clear the persisted membership flag so a restart doesn't resurrect it.
    {
        let snapshot = {
            let mut map = sessions.write().await;
            match map.get_mut(session_id) {
                Some(entry) => {
                    entry.is_swarm_member = false;
                    Some(entry.clone())
                }
                None => None,
            }
        };
        if let Some(snap) = snapshot {
            let _ = crate::storage::save_session(&snap).await;
        }
    }
    send(
        w,
        &Event::Stopped {
            id,
            session_id: session_id.to_string(),
        },
    )
    .await?;
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
    // Deterministic fallback when no session is named.
    let mut keys: Vec<&String> = sw.members.keys().collect();
    keys.sort();
    let target = session_id
        .map(|s| s.to_string())
        .or_else(|| keys.first().map(|s| (*s).clone()));
    if let Some(sid) = target.as_deref() {
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
async fn handle_swarm_list(w: &Socket, swarm: &SharedSwarm, id: u64) -> Result<()> {
    let sw = swarm.read().await;
    send(
        w,
        &Event::MemberList {
            id,
            members: sw.member_list(),
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_logic() {
        let ctx = ToolContext::new(
            std::path::PathBuf::from("/work/proj"),
            String::new(),
            "basic".to_string(),
            Vec::new(),
        );
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
        assert!(gate_tool(
            "glob",
            &serde_json::json!({"pattern": "**", "path": "../"}),
            &ctx
        )
        .is_some());
        // bash: in-workspace commands are ungated, escapes are gated.
        assert!(gate_tool("bash", &serde_json::json!({"command": "ls"}), &ctx).is_none());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cargo test"}), &ctx).is_none());
        assert!(gate_tool(
            "bash",
            &serde_json::json!({"command": "grep -r alpha src"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "bash",
            &serde_json::json!({"command": "cat /etc/hosts"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "bash",
            &serde_json::json!({"command": "ls ~/sysadmin-mcp"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool("bash", &serde_json::json!({"command": "cd .. && ls"}), &ctx).is_some());
        assert!(gate_tool(
            "bash",
            &serde_json::json!({"command": "cd /tmp && pwd"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "bash",
            &serde_json::json!({"command": "cat $HOME/.claude/settings.json"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool("plan", &serde_json::json!({"action": "show"}), &ctx).is_none());
        // Read-only git actions are ungated.
        assert!(gate_tool("git", &serde_json::json!({"action": "status"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "log"}), &ctx).is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "diff"}), &ctx).is_none());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "remote", "branch": "x"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool("git", &serde_json::json!({"action": "fetch"}), &ctx).is_none());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "stash", "stash_action": "list"}),
            &ctx
        )
        .is_none());
        // Creating a branch pointer at HEAD is ungated.
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "branch", "branch": "topic"}),
            &ctx
        )
        .is_none());
        // Mutating git actions are gated.
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "add", "path": "all"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "commit", "message": "x"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "push"}), &ctx).is_some());
        assert!(gate_tool("git", &serde_json::json!({"action": "pull"}), &ctx).is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "checkout", "branch": "x"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "branch", "branch": "old", "delete_branch": true}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "stash", "stash_action": "pop"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "merge", "branch": "main"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "rebase", "branch": "origin/main"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "git",
            &serde_json::json!({"action": "reset", "mode": "hard", "ref": "HEAD~1"}),
            &ctx
        )
        .is_some());
        // Web + notes.
        assert!(gate_tool(
            "fetch_url",
            &serde_json::json!({"url": "https://example.com"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool("note", &serde_json::json!({"action": "clear"}), &ctx).is_none());
        // http_request: read methods ungated, mutating gated.
        assert!(gate_tool(
            "http_request",
            &serde_json::json!({"method": "GET", "url": "https://a.com"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "http_request",
            &serde_json::json!({"method": "HEAD", "url": "https://a.com"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "http_request",
            &serde_json::json!({"method": "post", "url": "https://a.com", "json": {}}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "http_request",
            &serde_json::json!({"method": "DELETE", "url": "https://a.com/x"}),
            &ctx
        )
        .is_some());
        // docker: reads ungated, state changes gated.
        assert!(gate_tool("docker", &serde_json::json!({"action": "ps"}), &ctx).is_none());
        assert!(gate_tool(
            "docker",
            &serde_json::json!({"action": "logs", "container": "web"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "docker",
            &serde_json::json!({"action": "inspect", "container": "web"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "docker",
            &serde_json::json!({"action": "exec", "container": "web", "command": "ls"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "docker",
            &serde_json::json!({"action": "run", "image": "nginx"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "docker",
            &serde_json::json!({"action": "rm", "container": "web", "force": true}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "docker",
            &serde_json::json!({"action": "build", "tag": "x"}),
            &ctx
        )
        .is_some());
        // sql: reads ungated, writes gated.
        assert!(gate_tool("sql", &serde_json::json!({"query": "SELECT 1"}), &ctx).is_none());
        assert!(gate_tool("sql", &serde_json::json!({"query": "show tables"}), &ctx).is_none());
        assert!(gate_tool(
            "sql",
            &serde_json::json!({"query": "INSERT INTO t VALUES (1)"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool("sql", &serde_json::json!({"query": "DELETE FROM t"}), &ctx).is_some());
        // Read-only SQL still gates on an outside-workspace sqlite file.
        assert!(gate_tool(
            "sql",
            &serde_json::json!({"query": "SELECT 1", "db": "app.db"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "sql",
            &serde_json::json!({"query": "SELECT 1", "db": "/etc/app.db"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "sql",
            &serde_json::json!({"query": "SELECT 1", "db": "postgres://u:p@host/db"}),
            &ctx
        )
        .is_none());

        // jq: inline text and in-workspace files are ungated; outside paths gate.
        assert!(gate_tool(
            "jq",
            &serde_json::json!({"filter": ".", "json": "{\"a\":1}"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "jq",
            &serde_json::json!({"filter": ".", "path": "data.json"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "jq",
            &serde_json::json!({"filter": ".", "path": "/etc/passwd"}),
            &ctx
        )
        .is_some());

        // gh: reads ungated, remote mutations gated.
        assert!(gate_tool("gh", &serde_json::json!({"action": "pr_list"}), &ctx).is_none());
        assert!(gate_tool("gh", &serde_json::json!({"action": "run_view"}), &ctx).is_none());
        assert!(gate_tool("gh", &serde_json::json!({"action": "pr_merge"}), &ctx).is_some());
        assert!(gate_tool("gh", &serde_json::json!({"action": "api"}), &ctx).is_some());

        // test: known runners ungated, custom commands gated.
        assert!(gate_tool("test", &serde_json::json!({"runner": "cargo"}), &ctx).is_none());
        assert!(gate_tool("test", &serde_json::json!({"runner": "pytest"}), &ctx).is_none());
        assert!(gate_tool(
            "test",
            &serde_json::json!({"runner": "custom", "args": ["make", "test"]}),
            &ctx
        )
        .is_some());

        // sandbox: list/exec ungated; create/run/copy/destroy gated.
        assert!(gate_tool("sandbox", &serde_json::json!({"action": "list"}), &ctx).is_none());
        assert!(gate_tool(
            "sandbox",
            &serde_json::json!({"action": "exec", "sandbox": "x", "command": "ls"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "sandbox",
            &serde_json::json!({"action": "run", "image": "nginx", "command": "nginx -t"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "sandbox",
            &serde_json::json!({"action": "create", "image": "nginx"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "sandbox",
            &serde_json::json!({"action": "destroy", "sandbox": "x"}),
            &ctx
        )
        .is_some());

        // ssh: remote code execution is always gated.
        assert!(gate_tool(
            "ssh",
            &serde_json::json!({"host": "server1", "command": "uptime"}),
            &ctx
        )
        .is_some());

        // ssh_copy: remote/local writes are always gated.
        assert!(gate_tool(
            "ssh_copy",
            &serde_json::json!({"action": "copy_to", "host": "server1", "path": "a.conf", "remote_path": "/etc/nginx/a.conf"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "ssh_copy",
            &serde_json::json!({"action": "copy_from", "host": "server1", "path": "out.log", "remote_path": "/var/log/nginx/error.log"}),
            &ctx
        )
        .is_some());

        // download writes to the workspace; jenkins reads are ungated, artifact
        // download gates.
        assert!(gate_tool(
            "download",
            &serde_json::json!({"url": "https://x/y.zip", "path": "y.zip"}),
            &ctx
        )
        .is_some());
        assert!(gate_tool(
            "jenkins",
            &serde_json::json!({"action": "console", "base_url": "https://j", "job": "a"}),
            &ctx
        )
        .is_none());
        assert!(gate_tool(
            "jenkins",
            &serde_json::json!({"action": "artifact", "base_url": "https://j", "job": "a", "artifact": "a.zip", "path": "a.zip"}),
            &ctx
        )
        .is_some());
    }

    #[test]
    fn security_policy_rules() {
        let sec = crate::config::SecurityConfig {
            allow_tools: vec!["edit".to_string()],
            deny_tools: vec!["docker".to_string()],
            allow_bash_patterns: vec!["^cargo test".to_string()],
            allow_hosts: vec!["server1".to_string()],
        };
        assert!(policy_allows("edit", &serde_json::json!({}), &sec));
        assert!(policy_allows(
            "bash",
            &serde_json::json!({"command": "cargo test -p x"}),
            &sec
        ));
        assert!(!policy_allows(
            "bash",
            &serde_json::json!({"command": "rm -rf /"}),
            &sec
        ));
        assert!(policy_allows(
            "ssh",
            &serde_json::json!({"host": "server1"}),
            &sec
        ));
        assert!(!policy_allows(
            "ssh",
            &serde_json::json!({"host": "other"}),
            &sec
        ));
        assert!(!policy_allows("read", &serde_json::json!({}), &sec));
    }

    #[test]
    fn base64_encoding_matches_reference() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn token_estimate_counts_content_and_tool_args() {
        let msgs = vec![
            Message {
                role: "user".to_string(),
                content: "abcd".to_string(),
                timestamp_ms: 0,
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: "assistant".to_string(),
                content: String::new(),
                timestamp_ms: 0,
                tool_calls: Some(vec![crate::storage::StoredToolCall {
                    id: "1".to_string(),
                    name: "read".to_string(),
                    arguments: "{\"path\":\"x\"}".to_string(),
                }]),
                tool_call_id: None,
            },
        ];
        // (4 + 0)/4 + (0 + 12)/4 = 1 + 3 = 4
        assert_eq!(estimate_messages_tokens(&msgs), 4);
    }

    #[tokio::test]
    async fn image_files_become_data_urls() {
        let dir = std::env::temp_dir().join(format!("jancode-img-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("pic.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        let urls = load_image_data(dir.to_str().unwrap(), &["pic.png".to_string()])
            .await
            .unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("data:image/png;base64,"));
        assert!(urls[0].ends_with("=") || urls[0].len() > "data:image/png;base64,".len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
