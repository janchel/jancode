use crate::config::runtime_dir;
use crate::protocol::{Event, Request};
use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, stdin};
use tracing::info;

/// Left-gutter border for model responses (interactive readability). Rendered
/// client-side only — no extra tokens or provider calls. The ANSI dim (`\x1b[2m`)
/// is only emitted on a real terminal, so piped/redirected output stays clean.
const RESPONSE_GUTTER_ANSI: &str = "\x1b[2m│\x1b[0m ";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// True when the style draws any response chrome (blank lines / gutter / box).
fn border_enabled(style: &str) -> bool {
    !matches!(style, "none" | "off" | "false")
}

/// True when the style adds top/bottom rules around the reply.
fn border_is_box(style: &str) -> bool {
    matches!(style, "box" | "boxed")
}

/// Choose the gutter prefix for the current output target. Returns an empty
/// string when the border is disabled, or when stdout isn't a terminal (so
/// `jancode ... | grep` and log files stay unpolluted).
fn gutter_prefix(style: &str) -> &'static str {
    if border_enabled(style) && std::io::stdout().is_terminal() {
        RESPONSE_GUTTER_ANSI
    } else {
        ""
    }
}

/// Wrap a status line in the dim SGR so `[tool]`/`[approval]`/`[thinking]`
/// recede behind the model's reply. Terminal-only (stderr), so piped logs stay
/// clean. Cheap: one small String per status line.
fn dim(s: &str, enabled: bool) -> String {
    if enabled && std::io::stderr().is_terminal() {
        format!("{}{}{}", ANSI_DIM, s, ANSI_RESET)
    } else {
        s.to_string()
    }
}

/// Terminal width for box rules: `$COLUMNS` when sane, else 60. Avoids pulling
/// in a terminal-size crate (keeps the dependency surface lean).
fn term_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w >= 10)
        .unwrap_or(60)
}

/// A full-width horizontal rule for box style.
fn rule_line() -> String {
    "─".repeat(term_width())
}

/// Prefix each line of a streamed response chunk with the gutter. `at_line_start`
/// tracks the cursor so the bar is emitted once per line and blank lines stay
/// unbarred. State is tracked even when the gutter is empty, so the caller can
/// decide whether to append a trailing newline. Pure function (unit-tested).
fn apply_gutter(text: &str, at_line_start: &mut bool, gutter: &str) -> String {
    // Fast path: no gutter and no line break -> nothing to prefix.
    if gutter.is_empty() && !text.contains('\n') {
        if !text.is_empty() {
            *at_line_start = false;
        }
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + gutter.len());
    for ch in text.chars() {
        if ch == '\n' {
            out.push('\n');
            *at_line_start = true;
        } else {
            if *at_line_start && !gutter.is_empty() {
                out.push_str(gutter);
            }
            *at_line_start = false;
            out.push(ch);
        }
    }
    out
}

/// Ask the daemon to probe the configured MCP servers (connect + initialize +
/// list tools) and return their status. Used by `/mcp`, `/mcp_status`,
/// `/mcp_tools`, and `/mcp_reconnect`. Returns an empty vec on timeout.
async fn mcp_probe_servers(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<Vec<crate::mcp::McpServerProbe>> {
    let id = crate::protocol::new_message_id();
    let req = Request::McpProbe { id };
    let data = serde_json::to_string(&req)?;
    w.write_all(data.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    let res = tokio::time::timeout(Duration::from_secs(20), async {
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
        Ok(Vec::new())
    })
    .await;
    match res {
        Ok(Ok(servers)) => Ok(servers),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(Vec::new()),
    }
}

/// Print per-server MCP connection status. Returns how many servers are online
/// so callers can add guidance when some are offline.
fn print_mcp_status(servers: &[crate::mcp::McpServerProbe]) -> (usize, usize) {
    let mut online = 0usize;
    for s in servers {
        let status = if s.ok { "ok" } else { "OFFLINE" };
        if s.ok {
            online += 1;
        }
        println!(
            "  - {} ({}, {}) [{}] - {} tools",
            s.name, s.url, s.transport, status, s.tools.len()
        );
        if let Some(e) = &s.error {
            println!("      error: {}", e.lines().next().unwrap_or(""));
        }
    }
    (online, servers.len())
}

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
            provider: None,
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

    // Ctrl+C never kills the chat: while a request is streaming it cancels the
    // request (sends `Request::Cancel` to the daemon); at the idle prompt it is
    // drained into a no-op. Quit with /q, /quit, /exit, or Ctrl+D.
    let mut interrupt =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let mut tools_enabled = true;
    // MCP tools are opt-in per turn (only `/mcp <message>` sets this). Ordinary
    // turns skip MCP entirely so a slow/down server can't slow every message.
    let mut mcp_requested = false;
    let mut session_id = Some(format!("connect-{}", crate::protocol::new_message_id()));
    println!(
        "swarm session id: {} (spawn agents with `jancode swarm spawn --parent {}`)",
        session_id.as_deref().unwrap_or(""),
        session_id.as_deref().unwrap_or("")
    );
    let default_model = crate::config::load()
        .map(|c| crate::config::resolve_provider(&c, None).default_model.clone())
        .unwrap_or_else(|_| "default".to_string());
    // Whether to print the model's reasoning as `[thinking]` lines. Defaults
    // to hidden for a clean CLI; enable with `[server] show_thinking = true`.
    let show_thinking = crate::config::load()
        .map(|c| c.server.show_thinking)
        .unwrap_or(false);
    // Left gutter/box for model responses (see `[server] response_border`).
    // Makes the AI's replies easy to spot between tool output and user input.
    let response_border = crate::config::load()
        .map(|c| c.server.response_border.clone())
        .unwrap_or_else(|_| "gutter".to_string());
    // Dim the [tool]/[approval]/[thinking] status lines so the reply stands out.
    let dim_tools = crate::config::load()
        .map(|c| c.server.dim_tool_lines)
        .unwrap_or(true);
    let gutter = gutter_prefix(&response_border);
    let chrome = border_enabled(&response_border);
    let boxed = border_is_box(&response_border);
    let mut current_model: Option<String> = None;
    let mut current_provider: Option<String> = None;
    let mut models_cache: Option<Vec<String>> = None;
    let mut sessions_cache: Vec<crate::storage::Session> = Vec::new();
    // Track the last user prompt and whether its turn failed (e.g. provider
    // rate limit). When the user switches model/provider after a failure, we
    // offer to re-send the last prompt so the new model can continue.
    let mut last_prompt: Option<String> = None;
    let mut last_turn_failed = false;
    // When set (after a provider/model switch following a failed turn), the
    // loop sends this prompt instead of waiting for new input.
    let mut retry_prompt: Option<String> = None;

    // Async stdin reader for non-blocking input
    let mut stdin = BufReader::new(stdin());

    loop {
        let prompt_label = current_model.as_deref().unwrap_or(&default_model);
        print!("{}> ", prompt_label);
        std::io::stdout().flush()?;
        let mut raw_input = String::new();
        // If a retry was queued (after a provider/model switch following a
        // failed turn), send it without waiting for new input.
        if let Some(rp) = &retry_prompt {
            raw_input = rp.clone();
            retry_prompt = None;
        } else {
            let n = stdin.read_line(&mut raw_input).await?;
            if n == 0 {
                // EOF (Ctrl+D): quit the chat cleanly instead of looping forever.
                println!();
                break;
            }
        }
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
            if remainder.is_empty() {
                // Bare `/mcp`: list configured servers AND check connectivity so
                // offline servers are visible immediately.
                println!("MCP servers configured:");
                for s in &cfg.mcp.servers {
                    println!("  - {} ({}, {})", s.name, s.url, s.transport);
                }
                println!();
                println!("checking connectivity...");
                let servers = mcp_probe_servers(&mut w, &mut lines).await?;
                let (online, total) = print_mcp_status(&servers);
                if online < total {
                    println!();
                    println!(
                        "hint: {} of {} MCP server(s) OFFLINE. Fix the server/config, then run \
                         /mcp_reconnect, or just send `/mcp <message>` (it reconnects on demand).",
                        total - online,
                        total
                    );
                }
                continue;
            }
            // /mcp <message>: enable tools INCLUDING MCP tools for this turn only.
            tools_enabled = true;
            mcp_requested = true;
            println!("tools enabled: true (MCP + built-in)");
            input = remainder;
        }
        if input == "/model" || input.starts_with("/model ")
            || input == "/models" || input.starts_with("/models ")
        {
            let cfg = crate::config::load().unwrap_or_default();
            // `/models` is an accepted alias for `/model`.
            let mut remainder = input
                .strip_prefix("/model")
                .or_else(|| input.strip_prefix("/models"))
                .unwrap_or("")
                .trim()
                .to_string();

            // /model grep <term> and /model search <term> narrow the catalog
            // instead of being treated as literal model ids.
            let mut filter: Option<String> = None;
            for kw in ["grep ", "search "] {
                if let Some(t) = remainder.strip_prefix(kw) {
                    filter = Some(t.trim().to_string());
                    remainder = String::new();
                    break;
                }
            }

            // Build the model catalog: config list, else live `GET /models`.
            let catalog = match &models_cache {
                Some(l) => l.clone(),
                None => {
                    let l = crate::provider::list_models(&cfg, current_provider.as_deref())
                        .await
                        .unwrap_or_else(|e| {
                            eprintln!("note: could not fetch model list from provider: {}", e);
                            Vec::new()
                        });
                    models_cache = Some(l.clone());
                    l
                }
            };
            // De-duplicate while preserving order.
            let mut seen = std::collections::HashSet::new();
            let catalog: Vec<String> = catalog
                .into_iter()
                .filter(|m| seen.insert(m.clone()))
                .collect();

            if catalog.is_empty() && filter.is_some() {
                println!("no models available (provider /models failed and no models list in config.toml)");
                continue;
            }

            // Narrow the catalog with the filter term if one was given.
            let show: Vec<String> = match &filter {
                Some(term) if !term.is_empty() => catalog
                    .iter()
                    .filter(|m| m.to_lowercase().contains(&term.to_lowercase()))
                    .cloned()
                    .collect(),
                _ => catalog.clone(),
            };
            if show.is_empty() && filter.is_some() {
                println!("no models match '{}' (run /model grep <term>)", filter.as_deref().unwrap_or(""));
                continue;
            }

            // Resolve a 1-based selection to a model. 0 = default (None).
            let resolve = |n: usize, list: &[String]| -> Option<Option<String>> {
                if n == 0 {
                    return Some(None);
                }
                list.get(n.saturating_sub(1)).cloned().map(Some)
            };

            if remainder.is_empty() {
                if catalog.is_empty() {
                    println!("no models available (provider /models failed and no models list in config.toml)");
                    println!("usage: /model <name>, or add a models list under [provider]");
                    continue;
                }
                match &filter {
                    Some(term) if !term.is_empty() => {
                        println!("models matching '{}' ({}):", term, show.len());
                    }
                    _ => println!("available models ({}) (narrow with /model grep <term>):", show.len()),
                }
                for (i, m) in show.iter().enumerate() {
                    println!("  [{}] {}", i + 1, m);
                }
                println!("  [0] default ({})", default_model);
                print!("select model number: ");
                std::io::stdout().flush()?;
                let mut pick = String::new();
                stdin.read_line(&mut pick).await?;
                match pick.trim().parse::<usize>() {
                    Ok(0) => {
                        current_model = None;
                        println!("model reset to default ({})", default_model);
                    }
                    Ok(n) => match resolve(n, &show) {
                        Some(Some(m)) => {
                            current_model = Some(m.clone());
                            println!("model switched to: {}", m);
                        }
                        Some(None) => unreachable!(),
                        None => println!("no model at number {} (run /model to list)", n),
                    },
                    Err(_) => println!("invalid selection: {}", pick.trim()),
                }
                continue;
            }

            // /model <number>: select from the catalog.
            if let Ok(n) = remainder.parse::<usize>() {
                match resolve(n, &catalog) {
                    Some(Some(m)) => {
                        current_model = Some(m.clone());
                        println!("model switched to: {}", m);
                        if last_turn_failed {
                            if let Some(lp) = &last_prompt {
                                eprint!("retry last prompt with new model? [y/N] ");
                                std::io::stdout().flush()?;
                                let mut ans = String::new();
                                let _ = stdin.read_line(&mut ans).await;
                                if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                                    retry_prompt = Some(lp.clone());
                                    last_turn_failed = false;
                                }
                            }
                        }
                    }
                    Some(None) => {
                        current_model = None;
                        println!("model reset to default ({})", default_model);
                    }
                    None => println!("no model at number {} (run /model to list)", n),
                }
                continue;
            }

            // /model <name>: set an explicit model id regardless of the catalog.
            if !catalog.is_empty() && !catalog.iter().any(|m| m == &remainder) {
                println!("note: '{}' is not in the fetched model list; the provider may reject it", remainder);
            }
            current_model = Some(remainder.clone());
            println!("model switched to: {}", remainder);
            if last_turn_failed {
                if let Some(lp) = &last_prompt {
                    eprint!("retry last prompt with new model? [y/N] ");
                    std::io::stdout().flush()?;
                    let mut ans = String::new();
                    let _ = stdin.read_line(&mut ans).await;
                    if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                        retry_prompt = Some(lp.clone());
                        last_turn_failed = false;
                    }
                }
            }
            continue;
        }
        if input == "/provider" || input.starts_with("/provider ") {
            let cfg = crate::config::load().unwrap_or_default();
            let names = crate::config::provider_names(&cfg);
            let remainder = input.strip_prefix("/provider").unwrap_or("").trim().to_string();
            if remainder.is_empty() {
                // List providers.
                println!("available providers ({}):", names.len());
                for (i, n) in names.iter().enumerate() {
                    let marker = if current_provider.as_deref().unwrap_or("") == n || (current_provider.is_none() && i == 0) {
                        " *"
                    } else {
                        ""
                    };
                    println!("  [{}] {}{}", i + 1, n, marker);
                }
                println!("  [0] default");
                println!("usage: /provider <name> or /provider <number>");
            } else {
                // Switch provider by name or number.
                let mut switched: Option<String> = None;
                if let Ok(n) = remainder.parse::<usize>() {
                    if n == 0 {
                        switched = Some("default".to_string());
                    } else if n >= 1 && n <= names.len() {
                        switched = Some(names[n - 1].clone());
                    }
                } else {
                    if names.iter().any(|x| x == &remainder) {
                        switched = Some(remainder.clone());
                    }
                }
                if let Some(p) = switched {
                    current_provider = if p == "default" { None } else { Some(p.clone()) };
                    current_model = None; // reset model; provider may differ
                    models_cache = None;  // refetch catalog for the new provider
                    println!("provider switched to: {}", if p == "default" { "default" } else { &p });
                    // If the previous turn failed (e.g. rate limit), offer to
                    // re-send the last prompt with the new provider.
                    if last_turn_failed {
                        if let Some(lp) = &last_prompt {
                            eprint!("retry last prompt with new provider? [y/N] ");
                            std::io::stdout().flush()?;
                            let mut ans = String::new();
                            let _ = stdin.read_line(&mut ans).await;
                            if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                                retry_prompt = Some(lp.clone());
                                last_turn_failed = false;
                            }
                        }
                    }
                } else {
                    println!("unknown provider '{}' (run /provider to list)", remainder);
                }
            }
            continue;
        }
        if input == "/mcp_tools" || input == "/mcp_status" || input == "/mcp_reconnect" || input == "/mcp_restart" {
            let show_tools = input == "/mcp_tools";
            let reconnect = input == "/mcp_reconnect" || input == "/mcp_restart";
            if reconnect {
                println!("reconnecting to MCP servers (reloading config)...");
            }
            let servers = mcp_probe_servers(&mut w, &mut lines).await?;
            if servers.is_empty() {
                println!("no MCP servers configured, or the daemon timed out waiting for MCP status");
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
                let (online, total) = print_mcp_status(&servers);
                if online < total {
                    println!();
                    println!(
                        "hint: {} of {} MCP server(s) OFFLINE. Fix the server/config, then run /mcp_reconnect.",
                        total - online,
                        total
                    );
                }
            }
            continue;
        }
        if input == "/session" {
            let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let all = crate::storage::list_sessions().await
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
                    current_model = Some(s.model.clone());
                    println!("resumed session: {} ({} messages, model: {})", s.title, s.messages.len(), s.model);
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
            println!("Available commands: /quit, /exit, /q (quit), /tools (toggle tool calling), /model (list models and switch), /provider (list/switch providers), /mcp (list MCP servers / send with MCP tools), /mcp_tools (list tools exposed by MCP servers), /mcp_status (MCP server connection status), /mcp_reconnect (reload config + reconnect MCP servers), /session (list sessions), /resume <number>, /memory (list memories), /forget <number>, /help (Ctrl+C cancels the running request; Ctrl+D quits)");
            continue;
        }
        // Drain any queued Ctrl+C first so a stray keypress while idle can't
        // cancel the request we're about to send.
        while tokio::time::timeout(std::time::Duration::from_millis(1), interrupt.recv())
            .await
            .is_ok()
        {}

        let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
        let id = crate::protocol::new_message_id();
        // Remember this prompt so we can re-send it after a model/provider
        // switch if the turn fails (e.g. rate limit).
        last_prompt = Some(input.clone());
        last_turn_failed = false;
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
                    "git".to_string(),
                    "fetch_url".to_string(),
                    "http_request".to_string(),
                    "note".to_string(),
                    "docker".to_string(),
                    "sql".to_string(),
                ])
            } else {
                None
            },
            model: current_model.clone(),
            provider: current_provider.clone(),
            cwd,
            interactive: true,
            // MCP tools only for turns started with `/mcp <message>`.
            mcp: mcp_requested,
        };
        // MCP opt-in is per-turn: reset so the next message is built-in only.
        mcp_requested = false;
        let data = serde_json::to_string(&req)?;
        w.write_all(data.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;

        let mut in_stream = false;
        let mut cancelled = false;
        // Tracks line starts so the response gutter is drawn once per line and
        // blank lines stay unbarred. Reset each turn.
        let mut resp_at_line_start = true;
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let line = match line {
                        Ok(Some(l)) => l,
                        _ => break,
                    };
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
                                } else if c.name == "git" {
                                    let action = c.input.get("action").and_then(|v| v.as_str()).unwrap_or("");
                                    let target: String = if matches!(action, "checkout" | "branch" | "push") {
                                        c.input.get("branch").and_then(|v| v.as_str())
                                            .or_else(|| c.input.get("refspec").and_then(|v| v.as_str()))
                                            .unwrap_or_default().to_string()
                                    } else if matches!(action, "merge" | "rebase") {
                                        match c.input.get("branch").and_then(|v| v.as_str()) {
                                            Some(b) => b.to_string(),
                                            None => {
                                                if action == "rebase"
                                                    && c.input.get("rebase_continue").and_then(|v| v.as_bool()).unwrap_or(false)
                                                {
                                                    "--continue".to_string()
                                                } else {
                                                    String::new()
                                                }
                                            }
                                        }
                                    } else if action == "reset" {
                                        c.input.get("ref").and_then(|v| v.as_str())
                                            .map(|s| format!("{} {}", c.input.get("mode").and_then(|v| v.as_str()).unwrap_or("mixed"), s))
                                            .unwrap_or_default()
                                    } else if action == "add" || action == "diff" {
                                        c.input.get("path").and_then(|v| v.as_str()).unwrap_or_default().to_string()
                                    } else {
                                        String::new()
                                    };
                                    if target.is_empty() {
                                        action.to_string()
                                    } else {
                                        format!("{} {}", action, target)
                                    }
                                } else if c.name == "fetch_url" {
                                    c.input.get("url").and_then(|v| v.as_str()).unwrap_or_default().to_string()
                                } else if c.name == "http_request" {
                                    let m = c.input.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
                                    let u = c.input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                    format!("{} {}", m, u)
                                } else if c.name == "note" {
                                    let a = c.input.get("action").and_then(|v| v.as_str()).unwrap_or("");
                                    let t = c.input.get("title").and_then(|v| v.as_str()).unwrap_or("");
                                    if t.is_empty() { a.to_string() } else { format!("{} {}", a, t) }
                                } else if c.name == "docker" {
                                    let a = c.input.get("action").and_then(|v| v.as_str()).unwrap_or("");
                                    let t = c.input.get("container").and_then(|v| v.as_str())
                                        .or_else(|| c.input.get("image").and_then(|v| v.as_str()))
                                        .unwrap_or("");
                                    if t.is_empty() { a.to_string() } else { format!("{} {}", a, t) }
                                } else if c.name == "sql" {
                                    c.input.get("query").and_then(|v| v.as_str()).unwrap_or("")
                                        .chars().take(60).collect::<String>()
                                } else {
                                    c.input.to_string()
                                };
                                eprintln!("{}", dim(&format!("[tool] {}", if arg_str.is_empty() { c.name.clone() } else { format!("{} {}", c.name, arg_str) }), dim_tools));
                            }
                            // A tool line ends with a newline on the shared
                            // terminal, so the next response line should start
                            // fresh (gutter included).
                            resp_at_line_start = true;
                        }
                        Event::ToolResult { .. } => {
                            // Suppress tool result output for a quiet, readable session.
                        }
                        Event::Status { id: _, message } => {
                            // Show reasoning/thinking progress only when
                            // `[server] show_thinking = true`. Other status
                            // messages (e.g. "Executing tool: ...") stay quiet.
                            if show_thinking && message.starts_with("thinking: ") {
                                eprintln!("{}", dim(&format!("[thinking] {}", message.strip_prefix("thinking: ").unwrap_or("")), dim_tools));
                                resp_at_line_start = true;
                            }
                        }
                        Event::TextDelta { id: _, text } => {
                            // On the first text of the turn, open the response
                            // block: a blank line for breathing room, and (box
                            // style) a top rule.
                            if !in_stream && !text.is_empty() {
                                in_stream = true;
                                if chrome {
                                    println!();
                                }
                                if boxed {
                                    println!("{}", rule_line());
                                }
                            }
                            let rendered = apply_gutter(&text, &mut resp_at_line_start, gutter);
                            print!("{}", rendered);
                            std::io::stdout().flush()?;
                        }
                        Event::ApprovalRequired { id, tool_call_id, tool_name, path, reason } => {
                            in_stream = false;
                            eprintln!();
                            eprintln!("{}", dim(&format!("[approval] {} — {}", tool_name, reason), dim_tools));
                            if let Some(p) = path {
                                eprintln!("{}", dim(&format!("           target: {}", p), dim_tools));
                            }
                            eprint!("allow? [y/N] ");
                            std::io::stdout().flush()?;
                            let mut ans = String::new();
                            let _ = stdin.read_line(&mut ans).await;
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
                            eprintln!("{}", dim(&format!("[approval] {}", if approved { "allowed" } else { "DENIED" }), dim_tools));
                            resp_at_line_start = true;
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
                            resp_at_line_start = true;
                        }
                        Event::Done { id: _ } => {
                            // Close the response block: finish the line, then
                            // (box style) a bottom rule, then a blank line.
                            if in_stream {
                                if !resp_at_line_start {
                                    println!();
                                }
                                if boxed {
                                    println!("{}", rule_line());
                                }
                                if chrome {
                                    println!();
                                }
                            }
                            break;
                        }
                        Event::Error { id: _, message } => {
                            eprintln!("error: {}", message);
                            last_turn_failed = true;
                            break;
                        }
                        _ => {}
                    }
                }
                _ = interrupt.recv() => {
                    if cancelled {
                        continue;
                    }
                    cancelled = true;
                    eprintln!("\n[ctrl-c] cancelling request...");
                    let req = Request::Cancel {
                        id,
                        session_id: session_id.clone(),
                    };
                    let data = serde_json::to_string(&req)?;
                    w.write_all(data.as_bytes()).await?;
                    w.write_all(b"\n").await?;
                    w.flush().await?;
                    // Keep draining events until the server's Done (from the
                    // aborted turn) so no stale events bleed into the next
                    // request's stream.
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)), if cancelled => {
                    // Safety net: the daemon should ack the abort promptly.
                    break;
                }
            }
        }
        if cancelled {
            println!("cancelled");
        }
    }
    Ok(())
}

pub async fn run_prompt(prompt: &str, model: Option<String>, tools: bool, mcp: bool) -> Result<()> {
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
                "git".to_string(),
                "fetch_url".to_string(),
                "http_request".to_string(),
                "note".to_string(),
                "docker".to_string(),
                "sql".to_string(),
            ])
        } else {
            None
        },
        // Opt-in via `--mcp`; MCP servers aren't connected otherwise.
        mcp,
        model,
        provider: None,
        cwd,
        interactive: false,
    };
    let data = serde_json::to_string(&req)?;
    w.write_all(data.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;

    let mut any_output = false;
    // Response chrome (terminal-only; see `[server] response_border`).
    let response_border = crate::config::load()
        .map(|c| c.server.response_border.clone())
        .unwrap_or_else(|_| "gutter".to_string());
    let dim_tools = crate::config::load()
        .map(|c| c.server.dim_tool_lines)
        .unwrap_or(true);
    let gutter = gutter_prefix(&response_border);
    let chrome = border_enabled(&response_border);
    let boxed = border_is_box(&response_border);
    let mut resp_at_line_start = true;
    while let Some(line) = lines.next_line().await? {
        let ev: Event = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match ev {
            Event::TextDelta { id: _, text } => {
                if !any_output && !text.is_empty() {
                    any_output = true;
                    if chrome {
                        println!();
                    }
                    if boxed {
                        println!("{}", rule_line());
                    }
                }
                let rendered = apply_gutter(&text, &mut resp_at_line_start, gutter);
                print!("{}", rendered);
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
} else if c.name == "git" {
                        let action = c.input.get("action").and_then(|v| v.as_str()).unwrap_or("");
                        let target: String = if matches!(action, "checkout" | "push") {
                            c.input.get("branch").and_then(|v| v.as_str())
                                .or_else(|| c.input.get("refspec").and_then(|v| v.as_str()))
                                .unwrap_or_default().to_string()
                        } else if matches!(action, "merge" | "rebase") {
                            match c.input.get("branch").and_then(|v| v.as_str()) {
                                Some(b) => b.to_string(),
                                None => {
                                    if action == "rebase"
                                        && c.input.get("rebase_continue").and_then(|v| v.as_bool()).unwrap_or(false)
                                    {
                                        "--continue".to_string()
                                    } else {
                                        String::new()
                                    }
                                }
                            }
                        } else if action == "reset" {
                            c.input.get("ref").and_then(|v| v.as_str())
                                .map(|s| format!("{} {}", c.input.get("mode").and_then(|v| v.as_str()).unwrap_or("mixed"), s))
                                .unwrap_or_default()
                        } else if action == "add" {
                            c.input.get("path").and_then(|v| v.as_str()).unwrap_or_default().to_string()
                        } else {
                            String::new()
                        };
                        if target.is_empty() {
                            action.to_string()
                        } else {
                            format!("{} {}", action, target)
                        }
                    } else if c.name == "fetch_url" {
                        c.input.get("url").and_then(|v| v.as_str()).unwrap_or_default().to_string()
                    } else if c.name == "http_request" {
                        let m = c.input.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
                        let u = c.input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        format!("{} {}", m, u)
                    } else if c.name == "note" {
                        let a = c.input.get("action").and_then(|v| v.as_str()).unwrap_or("");
                        let t = c.input.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        if t.is_empty() { a.to_string() } else { format!("{} {}", a, t) }
                    } else if c.name == "docker" {
                        let a = c.input.get("action").and_then(|v| v.as_str()).unwrap_or("");
                        let t = c.input.get("container").and_then(|v| v.as_str())
                            .or_else(|| c.input.get("image").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        if t.is_empty() { a.to_string() } else { format!("{} {}", a, t) }
                    } else if c.name == "sql" {
                        c.input.get("query").and_then(|v| v.as_str()).unwrap_or("")
                            .chars().take(60).collect::<String>()
                    } else {
                        c.input.to_string()
                    };
                    eprintln!("{}", dim(&format!("[tool] {}", if arg_str.is_empty() { c.name.clone() } else { format!("{} {}", c.name, arg_str) }), dim_tools));
                }
            }
            Event::ToolResult { .. } => {}
            Event::Status { .. } => {}
            Event::Done { id: _ } => {
                if any_output {
                    if !resp_at_line_start {
                        println!();
                    }
                    if boxed {
                        println!("{}", rule_line());
                    }
                    if chrome {
                        println!();
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutter_prefixes_each_line_once() {
        let g = "│ ";
        // First line gets a bar; the char after a newline gets a bar too.
        let mut at_start = true;
        assert_eq!(apply_gutter("hello", &mut at_start, g), "│ hello");
        assert!(!at_start);
        // Continuation of the same line: no extra bar.
        assert_eq!(apply_gutter(" world", &mut at_start, g), " world");
        // A newline then text -> bar on the new line.
        assert_eq!(apply_gutter("\nsecond", &mut at_start, g), "\n│ second");
        // Trailing newline leaves us at line start (blank lines stay unbarred).
        assert_eq!(apply_gutter("x\ny\n", &mut at_start, g), "x\n│ y\n");
        assert!(at_start);
    }

    #[test]
    fn gutter_blank_lines_unbarred() {
        let g = "│ ";
        let mut at_start = true;
        // A blank line between content must not get a bar.
        assert_eq!(apply_gutter("a\n\nb", &mut at_start, g), "│ a\n\n│ b");
    }

    #[test]
    fn gutter_disabled_is_passthrough() {
        let mut at_start = true;
        // Empty gutter -> exact passthrough, but line state is still tracked so
        // the Done handler knows whether to append a trailing newline.
        assert_eq!(apply_gutter("hello\nworld", &mut at_start, ""), "hello\nworld");
        assert!(!at_start);
        // Text ending in a newline leaves us at line start.
        assert_eq!(apply_gutter("done\n", &mut at_start, ""), "done\n");
        assert!(at_start);
        // Empty chunk doesn't disturb state.
        assert_eq!(apply_gutter("", &mut at_start, ""), "");
        assert!(at_start);
    }

    #[test]
    fn gutter_style_selection() {
        // "none"/"off"/"false" disable regardless of terminal.
        assert_eq!(gutter_prefix("none"), "");
        assert_eq!(gutter_prefix("off"), "");
        assert_eq!(gutter_prefix("false"), "");
        // Non-disable styles are terminal-aware; the value depends on whether
        // stdout is a TTY (true under `--nocapture`, false when captured), so we
        // only assert it's one of the two valid prefixes.
        let g = gutter_prefix("gutter");
        assert!(g == RESPONSE_GUTTER_ANSI || g.is_empty());
    }

    #[test]
    fn border_style_predicates() {
        // Enabled for gutter/box, disabled for none/off/false.
        assert!(border_enabled("gutter"));
        assert!(border_enabled("box"));
        assert!(!border_enabled("none"));
        assert!(!border_enabled("off"));
        assert!(!border_enabled("false"));
        // Box only for box/boxed.
        assert!(border_is_box("box"));
        assert!(border_is_box("boxed"));
        assert!(!border_is_box("gutter"));
        assert!(!border_is_box("none"));
    }

    #[test]
    fn dim_is_passthrough_when_disabled() {
        // Disabled -> exact passthrough regardless of terminal.
        assert_eq!(dim("hello", false), "hello");
    }

    #[test]
    fn rule_line_is_nonempty() {
        assert!(!rule_line().is_empty());
    }
}
