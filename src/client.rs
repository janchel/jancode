use crate::config::runtime_dir;
use crate::protocol::{Event, Request};
use anyhow::{Context, Result};
use std::io::Write;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::info;

/// Handle a `jancode swarm <subcommand>` CLI call.
pub async fn handle_swarm(sub: crate::SwarmCommands) -> Result<()> {
    let stream = connect_or_spawn().await?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    let sub_kind = match &sub {
        crate::SwarmCommands::Spawn { .. } => "spawn",
        crate::SwarmCommands::List => "list",
        crate::SwarmCommands::Status { .. } => "status",
        crate::SwarmCommands::Dm { .. } => "dm",
        crate::SwarmCommands::Stop { .. } => "stop",
    };

    let req = match sub {
        crate::SwarmCommands::Spawn {
            prompt,
            label,
            parent,
            model,
        } => Request::SwarmSpawn {
            id: crate::protocol::new_message_id(),
            parent_session_id: parent,
            initial_message: prompt,
            model,
            label,
        },
        crate::SwarmCommands::List => Request::SwarmList {
            id: crate::protocol::new_message_id(),
        },
        crate::SwarmCommands::Status { session } => Request::SwarmStatus {
            id: crate::protocol::new_message_id(),
            session_id: session,
        },
        crate::SwarmCommands::Dm { to, message } => Request::SwarmDM {
            id: crate::protocol::new_message_id(),
            from_session_id: None,
            to_session_id: to,
            message,
        },
        crate::SwarmCommands::Stop {
            force,
            session,
        } => Request::SwarmStop {
            id: crate::protocol::new_message_id(),
            session_id: session,
            force,
        },
    };

    let data = serde_json::to_string(&req)?;
    w.write_all(data.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;

    while let Some(line) = lines.next_line().await? {
        let ev: Event = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match &ev {
            Event::Spawned {
                id: _,
                new_session_id,
                label,
            } => {
                let lbl = label.as_deref().unwrap_or("(no label)");
                println!("spawned agent {} \"{}\"", new_session_id, lbl);
            }
            Event::MemberList { id: _, members } => {
                println!("swarm members ({}):", members.len());
                for m in members {
                    println!(
                        "  {} [{}] label={:?} headless={} parent={:?}",
                        m.session_id, m.status, m.label, m.is_headless, m.parent_session_id
                    );
                }
            }
            Event::MemberStatus {
                id: _,
                session_id,
                status,
                detail,
                task_label,
            } => {
                let d = detail.as_deref().unwrap_or("");
                let t = task_label.as_deref().unwrap_or("");
                println!("{}: {} (detail={}, task={})", session_id, status, d, t);
            }
            Event::Status { id: _, message } => {
                println!("status: {}", message);
            }
            Event::Notification {
                id: _,
                from_session,
                notification_type,
                message,
            } => {
                let from = from_session.as_deref().unwrap_or("system");
                println!("[{}] from {}: {}", notification_type_str(notification_type), from, message);
            }
            Event::Stopped { id: _, session_id } => {
                println!("stopped session: {}", session_id);
            }
            Event::Done { id: _ } => break,
            Event::Error { id: _, message } => {
                eprintln!("error: {}", message);
                anyhow::bail!(message.to_string());
            }
            Event::Ack { .. } => {}
            Event::Pong { .. } => {}
            Event::History { .. } => {}
            Event::McpInfo { .. } => {}
            Event::TextDelta { id: _, text: _ } => {}
            Event::ToolCall { .. } => {}
            Event::ToolResult { .. } => {}
            Event::ApprovalRequired { .. } => {}
        }
        // Exit once the expected response for this subcommand has been seen so
        // one-shot CLI calls return instead of hanging on the open connection.
        let done = match sub_kind {
            "spawn" => matches!(ev, Event::Spawned { .. }),
            "list" => matches!(ev, Event::MemberList { .. }),
            "status" => matches!(ev, Event::MemberStatus { .. }),
            "dm" => matches!(ev, Event::Notification { .. }),
            "stop" => matches!(ev, Event::Stopped { .. }),
            _ => false,
        };
        if done {
            break;
        }
    }
    Ok(())
}

fn notification_type_str(t: &crate::protocol::NotificationType) -> &'static str {
    match t {
        crate::protocol::NotificationType::Dm => "DM",
        crate::protocol::NotificationType::Broadcast => "broadcast",
        crate::protocol::NotificationType::CompletionReport => "report",
        crate::protocol::NotificationType::Lifecycle => "lifecycle",
        crate::protocol::NotificationType::Stopped => "stopped",
    }
}


/// Connect to the daemon, auto-spawning it if it is not already running.
/// Mirrors jancode's lazy shared-server model: `run` and `connect` start a
/// detached background daemon on first use instead of failing with
/// "is the server running?".
async fn connect_or_spawn() -> Result<tokio::net::UnixStream> {
    let socket = runtime_dir().join("jancode.sock");
    if socket.exists() {
        match tokio::net::UnixStream::connect(&socket).await {
            Ok(s) => return Ok(s),
            Err(_) => {}
        }
    }

    // Spawn a detached daemon that inherits stdout/stderr for logging but
    // detaches from this process so it survives after `run`/`connect` exits.
    let exe = std::env::current_exe().context("resolving jancode executable path")?;
    let log_path = std::env::temp_dir().join("jancode-daemon.log");
    let log_file = std::fs::File::create(&log_path)
        .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
    let log_file_err = log_file.try_clone().unwrap_or_else(|_| {
        std::fs::File::create("/dev/null").unwrap()
    });
    let mut cmd = Command::new(&exe);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file_err));
    // Forward all environment variables so the daemon can find the API key
    // regardless of which env var name the user configured in config.toml.
    for (key, val) in std::env::vars() {
        cmd.env(&key, &val);
    }
    let child = cmd.spawn().context("spawning jancode serve")?;
    drop(child);

    // Wait for the socket to appear (the daemon binds it on startup).
    for _ in 0..50 {
        if socket.exists() {
            if let Ok(s) = tokio::net::UnixStream::connect(&socket).await {
                return Ok(s);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("jancode daemon failed to start")
}

pub async fn connect() -> Result<()> {
    let stream = connect_or_spawn().await?;
    info!("connected to jancode daemon");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    let mut tools_enabled = true;
    let mut session_id = Some(format!("connect-{}", crate::protocol::new_message_id()));
    println!(
        "swarm session id: {} (spawn agents with `jancode swarm spawn --parent {}`)",
        session_id.as_deref().unwrap_or(""),
        session_id.as_deref().unwrap_or("")
    );
    let mut sessions_cache: Vec<crate::storage::Session> = Vec::new();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut raw_input = String::new();
        std::io::stdin().read_line(&mut raw_input)?;
        let mut input = raw_input.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" || input == "/q" {
            break;
        }
        if input == "/tools" || input.starts_with("/tools ") {
            let remainder = input.strip_prefix("/tools ").unwrap_or("").trim().to_string();
            if remainder.is_empty() {
                // Bare /tools: toggle
                tools_enabled = !tools_enabled;
                println!("tools enabled: {}", tools_enabled);
                continue;
            }
            // /tools <message>: force tools ON for this message
            tools_enabled = true;
            println!("tools enabled: {} (for this message)", tools_enabled);
            input = remainder;
        }
        if input == "/mcp" || input.starts_with("/mcp ") {
            let cfg = crate::config::load().unwrap_or_default();
            if cfg.mcp.servers.is_empty() {
                println!("no MCP servers configured; add [[mcp.servers]] to ~/.jancode/config.toml");
                println!("  [[mcp.servers]]\n  name = \"server-name\"\n  url = \"https://host/mcp\"");
                continue;
            }
            let remainder = input.strip_prefix("/mcp ").unwrap_or("").trim().to_string();
            println!("MCP servers configured:");
            for s in &cfg.mcp.servers {
                println!("  - {} ({}, {})", s.name, s.url, s.transport);
            }
            if remainder.is_empty() {
                continue;
            }
            // /mcp <message>: enable tools (which include MCP tools) and send.
            tools_enabled = true;
            println!("tools enabled: true (MCP + built-in)");
            input = remainder;
        }
        if input == "/mcp_tools" || input == "/mcp_status" {
            let show_tools = input == "/mcp_tools";
            let id = crate::protocol::new_message_id();
            let req = Request::McpProbe { id };
            let data = serde_json::to_string(&req)?;
            w.write_all(data.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
            let timeout = tokio::time::timeout(
                Duration::from_secs(20),
                async {
                    while let Some(line) = lines.next_line().await? {
                        let ev: Event = match serde_json::from_str(&line) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        if let Event::McpInfo { id: mid, servers } = ev {
                            if mid == id {
                                return Ok::<Vec<crate::mcp::McpServerProbe>, anyhow::Error>(servers);
                            }
                        }
                    }
                    Ok::<Vec<crate::mcp::McpServerProbe>, anyhow::Error>(Vec::new())
                },
            )
            .await;
            match timeout {
                Ok(Ok(servers)) => {
                    if servers.is_empty() {
                        println!("no MCP servers configured; add [[mcp.servers]] to ~/.jancode/config.toml");
                        println!("  [[mcp.servers]]\n  name = \"server-name\"\n  url = \"https://host/mcp\"");
                    } else if show_tools {
                        for s in &servers {
                            println!("--- {} ({}) ---", s.name, s.transport);
                            if s.tools.is_empty() {
                                println!("  (no tools exposed)");
                            } else {
                                for t in &s.tools {
                                    println!("  - {}", t.name);
                                    let d = t.description.trim();
                                    if !d.is_empty() {
                                        println!("      {}", d.lines().next().unwrap_or(""));
                                    }
                                }
                            }
                            println!();
                        }
                        println!("Tip: /mcp <message> sends with these tools enabled");
                    } else {
                        for s in &servers {
                            let status = if s.ok { "ok" } else { "FAILED" };
                            println!("  - {} ({}, {}) [{}] - {} tools",
                                s.name, s.url, s.transport, status, s.tools.len());
                            if let Some(e) = &s.error {
                                println!("      error: {}", e.lines().next().unwrap_or(""));
                            }
                        }
                    }
                }
                _ => println!("timed out waiting for MCP status from daemon"),
            }
            continue;
        }
        if input == "/session" {
            let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let all = crate::storage::list_sessions()
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.working_dir == cwd)
                .collect::<Vec<_>>();
            sessions_cache = all;
            if sessions_cache.is_empty() {
                println!("no saved sessions for {}", cwd);
                continue;
            }
            for (i, s) in sessions_cache.iter().enumerate() {
                println!("  [{}] {} | model: {} | msgs: {} | updated: {}",
                    i + 1, s.title, s.model, s.messages.len(),
                    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(s.updated_at_ms)
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "?".to_string()));
            }
            continue;
        }
        if input == "/resume" || input.starts_with("/resume ") {
            let num = input.strip_prefix("/resume").unwrap_or("").trim();
            if num.is_empty() {
                println!("usage: /resume <number> (see /session)");
                continue;
            }
            let idx: usize = match num.parse::<usize>() {
                Ok(n) if n >= 1 => n - 1,
                _ => {
                    println!("invalid session number: {}", num);
                    continue;
                }
            };
            if sessions_cache.is_empty() {
                println!("no session list; run /session first");
                continue;
            }
            match sessions_cache.get(idx) {
                Some(s) => {
                    session_id = Some(s.id.clone());
                    println!("resumed session: {} ({} messages)", s.title, s.messages.len());
                    for m in &s.messages {
                        println!("  [{}] {}", m.role, m.content.chars().take(120).collect::<String>());
                    }
                }
                None => println!("no session at number {}", num),
            }
            continue;
        }
        if input == "/memory" {
            let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let notes = crate::memory::load_notes()
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.folder == cwd)
                .collect::<Vec<_>>();
            if notes.is_empty() {
                println!("no memory notes for {}", cwd);
                continue;
            }
            for (i, n) in notes.iter().enumerate() {
                println!("  [{}] {} ({})", i + 1, n.text, n.id.get(..8).unwrap_or(&n.id));
            }
            continue;
        }
        if input == "/forget" || input.starts_with("/forget ") {
            let num = input.strip_prefix("/forget").unwrap_or("").trim();
            let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let notes = crate::memory::load_notes().unwrap_or_default();
            let notes_in_folder: Vec<&crate::memory::MemoryNote> = notes.iter().filter(|n| n.folder == cwd).collect();
            if num.is_empty() {
                println!("usage: /forget <number> (see /memory)");
                continue;
            }
            let idx: usize = match num.parse::<usize>() {
                Ok(n) if n >= 1 => n - 1,
                _ => {
                    println!("invalid memory number: {}", num);
                    continue;
                }
            };
            match notes_in_folder.get(idx) {
                Some(n) => {
                    let _ = crate::memory::remove_note(&n.id);
                    println!("forgot: {}", n.text);
                }
                None => println!("no memory at number {}", num),
            }
            continue;
        }
        if input == "/help" {
            println!("Available commands: /quit, /exit, /q (quit), /tools (toggle tool calling), /mcp (list MCP servers / send with MCP tools), /mcp_tools (list tools exposed by MCP servers), /mcp_status (MCP server connection status), /session (list sessions), /resume <number>, /memory (list memories), /forget <number>, /help");
            continue;
        }
        let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
        let id = crate::protocol::new_message_id();
        let req = Request::Message {
            id,
            session_id: session_id.clone(),
            content: input,
            tools: if tools_enabled {
                Some(vec![
                    "bash".to_string(),
                    "read".to_string(),
                    "write".to_string(),
                    "edit".to_string(),
                    "list_dir".to_string(),
                    "glob".to_string(),
                    "agentgrep".to_string(),
                    "apply_patch".to_string(),
                    "plan".to_string(),
                ])
            } else {
                None
            },
            model: None,
            cwd,
            interactive: true,
        };
        let data = serde_json::to_string(&req)?;
        w.write_all(data.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;

        let mut in_stream = false;
        while let Some(line) = lines.next_line().await? {
            let ev: Event = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            match ev {
                Event::Ack { .. } => {}
                Event::ToolCall { id: _, calls } => {
                    for c in &calls {
                        let arg_str = if c.name == "bash" {
                            c.input.get("command").and_then(|v| v.as_str())
                                .map(|s| s.chars().take(80).collect::<String>())
                                .unwrap_or_default()
                        } else if c.name == "read" {
                            c.input.get("path").and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_default()
                        } else if c.name == "write" || c.name == "edit" {
                            c.input.get("path").and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_default()
                        } else if c.name == "plan" {
                            c.input.get("action").and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_default()
                        } else {
                            c.input.to_string()
                        };
                        eprintln!("[tool] {}", if arg_str.is_empty() { c.name.clone() } else { format!("{} {}", c.name, arg_str) });
                    }
                }
                Event::ToolResult { .. } => {
                    // Suppress tool result output for a quiet, readable session.
                }
                Event::Status { .. } => {
                    // Suppress status/progress messages.
                }
                Event::TextDelta { id: _, text } => {
                    in_stream = true;
                    print!("{}", text);
                    std::io::stdout().flush()?;
                }
                Event::ApprovalRequired { id, tool_call_id, tool_name, path, reason } => {
                    in_stream = false;
                    eprintln!();
                    eprintln!("[approval] {} — {}", tool_name, reason);
                    if let Some(p) = path {
                        eprintln!("           target: {}", p);
                    }
                    eprint!("allow? [y/N] ");
                    std::io::stdout().flush()?;
                    let mut ans = String::new();
                    let _ = std::io::stdin().read_line(&mut ans);
                    let approved = matches!(ans.trim().to_lowercase().as_str(), "y" | "yes");
                    let resp = Request::ApprovalResponse {
                        id,
                        session_id: session_id.clone(),
                        tool_call_id,
                        approved,
                    };
                    let data = serde_json::to_string(&resp)?;
                    w.write_all(data.as_bytes()).await?;
                    w.write_all(b"\n").await?;
                    w.flush().await?;
                    eprintln!("[approval] {}", if approved { "allowed" } else { "DENIED" });
                }
                Event::Notification {
                    id: _,
                    from_session,
                    notification_type,
                    message,
                } => {
                    // Out-of-band swarm event (DM / broadcast / child-agent
                    // completion report). Show it so the parent sees agent
                    // activity live, without corrupting the streamed reply.
                    in_stream = false;
                    eprintln!();
                    let from = from_session.as_deref().unwrap_or("system");
                    eprintln!(
                        "[{}] from {}: {}",
                        notification_type_str(&notification_type),
                        from,
                        message
                    );
                }
                Event::Done { id: _ } => {
                    if in_stream {
                        println!();
                    }
                    break;
                }
                Event::Error { id: _, message } => {
                    eprintln!("error: {}", message);
                    break;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub async fn run_prompt(prompt: &str, model: Option<String>, tools: bool) -> Result<()> {
    let stream = connect_or_spawn().await?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    // Each `jancode run` starts a fresh session (one-shot, no resume).
    let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
    let id = crate::protocol::new_message_id();
    let req = Request::Message {
        id,
        session_id: None,
        content: prompt.to_string(),
        tools: if tools {
            Some(vec![
                "bash".to_string(),
                "read".to_string(),
                "write".to_string(),
                "edit".to_string(),
                "list_dir".to_string(),
                "glob".to_string(),
                "agentgrep".to_string(),
                "apply_patch".to_string(),
                "plan".to_string(),
            ])
        } else {
            None
        },
        model,
        cwd,
        interactive: false,
    };
    let data = serde_json::to_string(&req)?;
    w.write_all(data.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;

    let mut any_output = false;
    while let Some(line) = lines.next_line().await? {
        let ev: Event = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match ev {
            Event::TextDelta { id: _, text } => {
                any_output = true;
                print!("{}", text);
                std::io::stdout().flush()?;
            }
            Event::ToolCall { id: _, calls } => {
                for c in &calls {
                    let arg_str = if c.name == "bash" {
                        c.input.get("command").and_then(|v| v.as_str())
                            .map(|s| s.chars().take(80).collect::<String>())
                            .unwrap_or_default()
                    } else if c.name == "read" || c.name == "write" || c.name == "edit" {
                        c.input.get("path").and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_default()
                    } else {
                        c.input.to_string()
                    };
                    eprintln!("[tool] {}", if arg_str.is_empty() { c.name.clone() } else { format!("{} {}", c.name, arg_str) });
                }
            }
            Event::ToolResult { .. } => {}
            Event::Status { .. } => {}
            Event::Done { id: _ } => {
                if any_output {
                    println!();
                }
                break;
            }
            Event::Error { id: _, message } => {
                anyhow::bail!(message);
            }
            _ => {}
        }
    }
    Ok(())
}
