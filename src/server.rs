use crate::config::runtime_dir;
use crate::protocol::{Event, Request};
use crate::storage::{list_sessions, save_session, Session, Message};
use crate::swarm::{self, Interrupt, SwarmState};
use crate::tools::{is_outside, ToolContext};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
    for s in list_sessions()? {
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
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let cfg = crate::config::load()?;
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
                send(&mut w, &Event::Pong { id }).await?;
            }
            Request::McpProbe { id } => {
                let servers = crate::mcp::probe(&cfg).await;
                send(&mut w, &Event::McpInfo { id, servers }).await?;
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
                    &mut w,
                    &Event::History {
                        id,
                        messages: msgs,
                    },
                )
                .await?;
            }
            Request::Message { id, session_id, content, tools, model, cwd, interactive } => {
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
                    for t in crate::mcp::load_tools(&cfg).await {
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
                        model: model.clone().unwrap_or_else(|| cfg.provider.default_model.clone()),
                        messages: Vec::new(),
                        created_at_ms: chrono::Utc::now().timestamp_millis(),
                        updated_at_ms: chrono::Utc::now().timestamp_millis(),
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

                send(&mut w, &Event::Ack { id }).await?;
                send(&mut w, &Event::Status { id: Some(id), message: "Thinking...".to_string() }).await?;

                // Build the tool definitions to send to the provider.
                let tool_defs: Vec<crate::tools::ToolDefinition> = tool_registry
                    .as_ref()
                    .map(|r| r.all().iter().map(|t| t.to_definition()).collect())
                    .unwrap_or_default();
                let tool_defs_ref = if tool_defs.is_empty() { None } else { Some(tool_defs.as_slice()) };

                let ctx = crate::tools::ToolContext {
                    working_dir: std::path::PathBuf::from(&working_dir),
                };

                let mut done_sent = false;
                let mut loop_iteration = 0u32;
                const MAX_TOOL_LOOPS: u32 = 20;

                // Tool-calling loop: send message, execute tools, append results, repeat.
                loop {
                    loop_iteration += 1;
                    if loop_iteration > MAX_TOOL_LOOPS {
                        error!("tool-calling loop exceeded {} iterations, breaking", MAX_TOOL_LOOPS);
                        send(&mut w, &Event::Error {
                            id: Some(id),
                            message: format!("exceeded maximum tool-calling iterations ({})", MAX_TOOL_LOOPS),
                        }).await?;
                        break;
                    }
                    // Get current conversation for provider.
                    let msgs: Vec<crate::storage::Message> = {
                        let s = sessions.read().await;
                        s.get(&session_id_s).map(|e| e.messages.clone()).unwrap_or_default()
                    };

                    let session_model = {
                        let s = sessions.read().await;
                        s.get(&session_id_s)
                            .map(|e| e.model.clone())
                            .unwrap_or_else(|| cfg.provider.default_model.clone())
                    };

                    // Build memory context: recent facts relevant to the working
                    // dir are appended to the system prompt automatically.
                    let query = if msgs.len() >= 1 { content.clone() } else { String::new() };
                    let memory_context = crate::memory::retrieve(&query, &working_dir, 5)
                        .into_iter()
                        .map(|n| format!("- {}", n.text))
                        .collect::<Vec<_>>()
                        .join("\n");

                        let result = crate::provider::send_message(
                        &cfg, &session_model, &msgs, tool_defs_ref, &memory_context,
                    ).await;

                    match result {
                        Ok(events) => {
                            let mut assistant_text = String::new();
                            let mut tool_calls: Vec<crate::protocol::ToolCall> = Vec::new();

                            for ev in &events {
                                match ev {
                                    Event::TextDelta { id: _, text } => {
                                        assistant_text.push_str(text);
                                        send(&mut w, ev).await?;
                                    }
                                    Event::ToolCall { id: _, calls } => {
                                        for c in calls {
                                            send(&mut w, &Event::ToolCall {
                                                id: Some(id),
                                                calls: vec![c.clone()],
                                            }).await?;
                                        }
                                        tool_calls.extend(calls.clone());
                                    }
                                    Event::Done { id: _ } => {}
                                    _ => {
                                        send(&mut w, ev).await?;
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
                                            tool_calls: if stored_tcs.is_empty() { None } else { Some(stored_tcs) },
                                            tool_call_id: None,
                                        });
                                    }
                                    entry.updated_at_ms = chrono::Utc::now().timestamp_millis();
                                    let snapshot = entry.clone();
                                    if let Err(e) = crate::storage::save_session(&snapshot) {
                                        error!("saving session {}: {}", session_id_s, e);
                                    }
                                }
                            }

                            // If no tool calls, done.
                            if tool_calls.is_empty() {
                                send(&mut w, &Event::Done { id: Some(id) }).await?;
                                done_sent = true;
                                break;
                            }

                            // Execute each tool and append results as assistant messages.
                            let mut results: Vec<crate::protocol::ToolResultEntry> = Vec::new();
                            for tc in &tool_calls {
                                send(&mut w, &Event::Status { id: Some(id), message: format!("Executing tool: {}...", tc.name) }).await?;
                                if let Some(tool) = tool_registry.as_ref().and_then(|r| r.find(&tc.name)) {
                                    // Approval gate: file modifications and reads
                                    // outside the workspace require consent.
                                    let gate = gate_tool(&tc.name, &tc.input, &ctx);
                                    let (approved, deny_reason) = match &gate {
                                        None => (true, None),
                                        Some(g) => match cfg.server.approve_mode.as_str() {
                                            "auto" => (true, None),
                                            "deny" => (false, Some(g.reason.clone())),
                                            _ => {
                                                if interactive {
                                                    send(&mut w, &Event::ApprovalRequired {
                                                        id,
                                                        tool_call_id: tc.id.clone(),
                                                        tool_name: tc.name.clone(),
                                                        path: g.path.clone(),
                                                        reason: g.reason.clone(),
                                                    }).await?;
                                                    info!("asking approval for {} ({})", tc.name, g.reason);
                                                    let ok = await_approval(&mut lines, id, &tc.id).await;
                                                    (ok, if ok { None } else { Some(g.reason.clone()) })
                                                } else {
                                                    info!("auto-approving {} in non-interactive mode: {}", tc.name, g.reason);
                                                    (true, None)
                                                }
                                            }
                                        },
                                    };
                                    let result_entry = if approved {
                                        match tool.execute(&tc.input, &ctx).await {
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
                                        }
                                    } else {
                                        warn!("tool denied: {} ({})", tc.name, deny_reason.as_deref().unwrap_or("no approval"));
                                        crate::protocol::ToolResultEntry {
                                            tool_call_id: tc.id.clone(),
                                            output: format!("APPROVAL_DENIED: {}", deny_reason.as_deref().unwrap_or("no approval")),
                                            is_error: true,
                                        }
                                    };
                                    send(&mut w, &Event::ToolResult {
                                        id: Some(id),
                                        results: vec![result_entry.clone()],
                                    }).await?;
                                    results.push(result_entry);
                                } else {
                                    let err_result = crate::protocol::ToolResultEntry {
                                        tool_call_id: tc.id.clone(),
                                        output: format!("ERROR: tool '{}' not found", tc.name),
                                        is_error: true,
                                    };
                                    send(&mut w, &Event::ToolResult {
                                        id: Some(id),
                                        results: vec![err_result.clone()],
                                    }).await?;
                                    results.push(err_result);
                                }
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
                                    if let Err(e) = crate::storage::save_session(&snapshot) {
                                        error!("saving session {}: {}", session_id_s, e);
                                    }
                                }
                            }

                            // Loop continues — next iteration sends the tool results to provider.
                        }
                        Err(e) => {
                            eprintln!("provider error: {}", e);
                            tracing::error!("provider call failed: {}", e);
                            send(&mut w, &Event::Error {
                                id: Some(id),
                                message: e.to_string(),
                            }).await?;
                            break;
                        }
                    }
                }

                if !done_sent {
                    send(&mut w, &Event::Done { id: Some(id) }).await?;
                }
            }
            Request::Cancel { .. } => {}

            // ---- Swarm / multi-agent (jancode-style, in-process) ----
            Request::SwarmSpawn {
                id,
                parent_session_id,
                initial_message,
                model,
                label,
            } => {
                handle_swarm_spawn(
                    &mut w, id, &sessions, &swarm, &session_clients, parent_session_id,
                    &initial_message, model.as_deref(), label.as_deref(),
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
                    &mut w, &swarm, &session_clients, from_session_id.as_deref(),
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
                    &mut w, &swarm, &session_clients, from_session_id.as_deref(), &message,
                )
                .await?;
            }
            Request::SwarmStop {
                id,
                session_id,
                force,
            } => {
                handle_swarm_stop(&mut w, &swarm, id, &session_id, force).await?;
            }
            Request::SwarmStatus { id, session_id } => {
                handle_swarm_status(&mut w, &swarm, id, session_id.as_deref()).await?;
            }
            Request::SwarmList { id } => {
                handle_swarm_list(&mut w, &swarm, id).await?;
            }
            Request::ApprovalResponse { .. } => {}
        }
    }
    // Client disconnected: drop our socket registrations so notifications stop
    // flowing to a dead handle (the sessions stay as swarm members).
    {
        let mut map = session_clients.write().await;
        for sid in &registered_sessions {
            map.remove(sid);
        }
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
        _ => None,
    }
}

/// Read the socket until an `ApprovalResponse` matching this message + tool call
/// arrives (up to 5 minutes), then return the user's decision. Times out -> deny.
async fn await_approval(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    msg_id: u64,
    tool_call_id: &str,
) -> bool {
    let timeout = std::time::Duration::from_secs(300);
    let res = tokio::time::timeout(
        timeout,
        async {
            while let Some(line) = lines.next_line().await? {
                if let Ok(req) = serde_json::from_str::<Request>(&line) {
                    if let Request::ApprovalResponse {
                        id,
                        tool_call_id: tcid,
                        approved,
                        ..
                    } = req
                    {
                        if id == msg_id && tcid == tool_call_id {
                            return Ok::<bool, std::io::Error>(approved);
                        }
                    }
                }
            }
            Ok::<bool, std::io::Error>(false)
        },
    )
    .await;
    res.map(|r| r.unwrap_or(false)).unwrap_or(false)
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
        if let Err(e) = save_session(&snapshot) {
            error!("saving session {}: {}", session_id, e);
        } else {
            tracing::info!("saved session {} ({} messages)", session_id, snapshot.messages.len());
        }
    } else {
        tracing::warn!("session {} not found in map during persist", session_id);
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, ev: &Event) -> Result<()> {
    let data = serde_json::to_string(ev)?;
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
    };

    const MAX_TOOL_LOOPS: u32 = 20;
    let mut total_text = String::new();
    let mut iteration = 0u32;

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
                .unwrap_or_else(|| cfg.provider.default_model.clone())
        };

        let events = match crate::provider::send_message(cfg, &session_model, &msgs, tool_defs_ref, "").await {
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
                if let Err(e) = crate::storage::save_session(&snapshot) {
                    error!("saving session {}: {}", session_id, e);
                }
            }
        }

        if tool_calls.is_empty() {
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
                if let Err(e) = crate::storage::save_session(&snapshot) {
                    error!("saving session {}: {}", session_id, e);
                }
            }
        }
    }

    Ok(total_text)
}

/// Spawn a new headless agent session and run its initial message.
async fn handle_swarm_spawn(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    req_id: u64,
    sessions: &SessionMap,
    swarm: &SharedSwarm,
    session_clients: &SessionClients,
    parent_session_id: Option<String>,
    initial_message: &str,
    model_override: Option<&str>,
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
        let model = model_override.unwrap_or(&cfg.provider.default_model).to_string();
        let entry = map.entry(session_id.clone()).or_insert_with(|| Session {
            id: session_id.clone(),
            title: format!("agent-{}", new_id),
            working_dir,
            model,
            messages: Vec::new(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
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

    send(&mut *w, &Event::Spawned {
        id: req_id,
        new_session_id: session_id.clone(),
        label: label.map(String::from),
    })
    .await?;

    // Run the agent headlessly with the full tool set (auto-approved — no
    // human is attached). Returns the accumulated assistant text.
    let assistant_text = run_headless_agent(&cfg, sessions, &session_id).await?;

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
    w: &mut tokio::net::unix::OwnedWriteHalf,
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
        &mut *w,
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
    _w: &mut tokio::net::unix::OwnedWriteHalf,
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
    w: &mut tokio::net::unix::OwnedWriteHalf,
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
            &mut *w,
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
    send(&mut *w, &Event::Stopped { id, session_id: session_id.to_string() }).await?;
    Ok(())

}

/// Return the live status of one member, or the requesting session itself.
async fn handle_swarm_status(
    w: &mut tokio::net::unix::OwnedWriteHalf,
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
                &mut *w,
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
        &mut *w,
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
    w: &mut tokio::net::unix::OwnedWriteHalf,
    swarm: &SharedSwarm,
    id: u64,
) -> Result<()> {
    let sw = swarm.read().await;
    send(&mut *w, &Event::MemberList { id, members: sw.member_list() }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_logic() {
        let ctx = ToolContext { working_dir: std::path::PathBuf::from("/work/proj") };
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
        // bash / plan are ungated.
        assert!(gate_tool("bash", &serde_json::json!({"command": "ls"}), &ctx).is_none());
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
    }
}
