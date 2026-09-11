#!/usr/bin/env python3
"""Add parallel tool execution support to server.rs"""

# Read the file
with open('/home/devops/Projects/jancode/src/server.rs', 'r') as f:
    content = f.read()

# 1. Add futures import
content = content.replace(
    'use crate::config::runtime_dir;\nuse crate::protocol::{Event, Request};\nuse crate::storage::{list_sessions, save_session, Session, Message};\nuse crate::swarm::{self, Interrupt, SwarmState};\nuse crate::tools::{is_outside, ToolContext};\nuse anyhow::{Context, Result};\nuse serde_json::Value;',
    'use crate::config::runtime_dir;\nuse crate::protocol::{Event, Request};\nuse crate::storage::{list_sessions, save_session, Session, Message};\nuse crate::swarm::{self, Interrupt, SwarmState};\nuse crate::tools::{is_outside, ToolContext};\nuse anyhow::{Context, Result};\nuse futures::future::join_all;\nuse serde_json::Value;'
)

# 2. Add is_read_only_tool function after normalize_path_key
content = content.replace(
    'result\n}\n\n/// Run one full `Request::Message` turn for a session: stream the provider',
    'result\n}\n\n/// Check if a tool is read-only (safe for parallel execution).\n/// Returns true if the tool never mutates state and doesn\'t require approval for normal use.\nfn is_read_only_tool(name: &str, input: &Value) -> bool {\n    match name {\n        "fetch_url" => true,\n        "http_request" => {\n            let method = input.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_uppercase();\n            matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")\n        }\n        "note" => true,\n        "docker" => {\n            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");\n            matches!(action, "ps" | "images" | "logs" | "inspect" | "stats")\n        }\n        "sql" => {\n            let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");\n            crate::tools::sql_is_read_only(query)\n        }\n        "git" => {\n            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");\n            let mut mutating = matches!(\n                action,\n                "add"\n                    | "commit"\n                    | "push"\n                    | "pull"\n                    | "checkout"\n                    | "branch"\n                    | "stash"\n                    | "merge"\n                    | "rebase"\n                    | "reset"\n            );\n            if action == "branch"\n                && !input\n                    .get("delete_branch")\n                    .and_then(|v| v.as_bool())\n                    .unwrap_or(False)\n            {\n                mutating = false;\n            }\n            if action == "stash" {\n                let sa = input.get("stash_action").and_then(|v| v.as_str());\n                mutating = !matches!(sa, None | Some("list"));\n            }\n            !mutating\n        }\n        "read" | "list_dir" | "glob" | "agentgrep" => true,\n        "fetch_url" | "note" => true,\n        _ => false,\n    }\n}\n\n/// Run one full `Request::Message` turn for a session: stream the provider'
)

# 3. Replace the tool execution loop
old_loop = '''                // Execute each tool and append results as assistant messages.
                let mut results: Vec<crate::protocol::ToolResultEntry> = Vec::new();
                for tc in &tool_calls {
                    send(w, &Event::Status {
                        id: Some(id),
                        message: format!("Executing tool: {}...", tc.name),
                    })
                    .await?;
                    if let Some(tool) = tool_registry.as_ref().and_then(|r| r.find(&tc.name)) {
                        // Approval gate: mutations and reads outside the
                        // workspace require consent.
                        let gate = gate_tool(&tc.name, &tc.input, &ctx);
                        let (approved, deny_reason) = match &gate {
                            None => (true, None),
                            Some(g) => match cfg.server.approve_mode.as_str() {
                                "auto" => (true, None),
                                "deny" => (false, Some(g.reason.clone())),
                                _ => {
                                    if interactive {
                                        // Resolve path to absolute using working directory, then normalize
                                        // This ensures "index.html", "./index.html", "/abs/index.html" all map to same cache key
                                        let path_key = g.path.clone().unwrap_or_default();
                                        let path_key_for_log = path_key.clone();
                                        let resolved_path = if !path_key.is_empty() {
                                            let p = Path::new(&path_key);
                                            if p.is_relative() {
                                                ctx.working_dir.join(p).display().to_string()
                                            } else {
                                                path_key
                                            }
                                        } else {
                                            String::new()
                                        };
                                        let norm_key = normalize_path_key(&resolved_path);
                                        let norm_key_log = norm_key.clone();
                                        info!("approval check: tool={} path_key={} norm_key={} cache_size={} cache_contains={}", 
                                            tc.name, path_key_for_log, norm_key_log, approved_paths.len(), approved_paths.contains(&norm_key));
                                        if !norm_key.is_empty() && approved_paths.contains(&norm_key) {
                                            info!("auto-approving {} (already approved in this session): {}", tc.name, g.reason);
                                            (true, None)
                                        } else {
                                            send(w, &Event::ApprovalRequired {
                                                id,
                                                tool_call_id: tc.id.clone(),
                                                tool_name: tc.name.clone(),
                                                path: g.path.clone(),
                                                reason: g.reason.clone(),
                                            })
                                            .await?;
                                            info!("asking approval for {} ({})", tc.name, g.reason);
                                            // The read loop routes ApprovalResponse
                                            // here over `approvals`.
                                            let key = format!("{}:{}", id, tc.id);
                                            let (tx, rx) = tokio::sync::oneshot::channel();
                                            approvals.write().await.insert(key.clone(), tx);
                                            let decision = tokio::time::timeout(
                                                std::time::Duration::from_secs(300),
                                                rx,
                                            )
                                            .await;
                                            approvals.write().await.remove(&key);
                                            let ok = matches!(decision, Ok(Ok(true)));
                                            if ok && !norm_key.is_empty() {
                                                let norm_key_clone = norm_key.clone();
                                                approved_paths.insert(norm_key);
                                                info!("inserted into approval cache: norm_key={} cache_size={}", norm_key_clone, approved_paths.len());
                                            }
                                            // Also extract paths from apply_patch patches to populate cache
                                            if ok && tc.name == "apply_patch" {
                                                info!("apply_patch approved, extracting paths from patch");
                                                if let Some(patch) = tc.input.get("patch").and_then(|v| v.as_str()) {
                                                    let extracted = extract_patch_paths(patch);
                                                    info!("extract_patch_paths returned: {:?}", extracted);
                                                    for p in extracted {
                                                        let p_clone = p.clone();
                                                        if !p_clone.is_empty() {
                                                            approved_paths.insert(p_clone.clone());
                                                            info!("inserted patch path into cache: {}", p_clone);
                                                        }
                                                    }
                                                } else {
                                                    info!("apply_patch has no patch field in input");
                                                }
                                            }
                                            (ok, if ok { None } else { Some(g.reason.clone()) })
                                        }
                                    } else {
                                        info!("auto-approving {} in non-interactive mode: {}", tc.name, g.reason);
                                        (true, None)
                                    }
                                }
                            }
                        };
                        let result_entry = if approved {
                            match tool.execute(&tc.input, &ctx).await {'''

new_loop = '''                // Execute tools with parallel read-only execution.
                let mut results: Vec<crate::protocol::ToolResultEntry> = Vec::new();

                // Group consecutive read-only tools for parallel execution
                let mut groups = Vec::new();
                let mut current_group = Vec::new();
                for tc in &tool_calls {
                    let read_only = is_read_only_tool(&tc.name, &tc.input);
                    if read_only {
                        current_group.push(tc.clone());
                    } else {
                        if !current_group.is_empty() {
                            groups.push((True, current_group));
                            current_group = Vec::new();
                        }
                        groups.push((False, vec![tc.clone()]));
                    }
                }
                if !current_group.is_empty() {
                    groups.push((True, current_group));
                }

                // Execute groups: parallel for read-only, sequential for mutating
                for (read_only, group) in groups {
                    if read_only and group.len() > 1:
                        # Parallel execution for read-only tools
                        let futures: Vec<_> = group.iter().map(|tc| {
                            let tool_name = tc.name.clone();
                            let tool_input = tc.input.clone();
                            let tool_id = tc.id.clone();
                            let tool_registry = tool_registry.clone();
                            let ctx = ctx.clone();
                            async move {
                                if let Some(tool) = tool_registry.as_ref().and_then(|r| r.find(&tool_name)) {
                                    # For parallel read-only tools, we auto-approve since they don't mutate
                                    match tool.execute(&tool_input, &ctx).await {
                                        Ok(s) => crate::protocol::ToolResultEntry {
                                            tool_call_id: tool_id,
                                            output: s,
                                            is_error: false,
                                        },
                                        Err(e) => crate::protocol::ToolResultEntry {
                                            tool_call_id: tool_id,
                                            output: format!("ERROR: {}", e),
                                            is_error: true,
                                        },
                                    }
                                } else {
                                    crate::protocol::ToolResultEntry {
                                        tool_call_id: tool_id,
                                        output: format!("ERROR: tool '{}' not found", tool_name),
                                        is_error: true,
                                    }
                                }
                            }
                        ).collect();
                        
                        let group_results = join_all(futures).await;
                        results.extend(group_results);
                    } else {
                        # Sequential execution for mutating tools or single read-only
                        for tc in group:
                            send(w, &Event::Status {
                                id: Some(id),
                                message: format!("Executing tool: {}...", tc.name),
                            })
                            .await?;
                            if let Some(tool) = tool_registry.as_ref().and_then(|r| r.find(&tc.name)) {
                                # Approval gate: mutations and reads outside the
                                # workspace require consent.
                                let gate = gate_tool(&tc.name, &tc.input, &ctx);
                                let (approved, deny_reason) = match &gate {
                                    None => (True, None),
                                    Some(g) => match cfg.server.approve_mode.as_str() {
                                        "auto" => (True, None),
                                        "deny" => (False, Some(g.reason.clone())),
                                        _ => {
                                            if interactive {
                                                # Resolve path to absolute using working directory, then normalize
                                                # This ensures "index.html", "./index.html", "/abs/index.html" all map to same cache key
                                                let path_key = g.path.clone().unwrap_or_default();
                                                let path_key_for_log = path_key.clone();
                                                let resolved_path = if !path_key.is_empty() {
                                                    let p = Path::new(&path_key);
                                                    if p.is_relative() {
                                                        ctx.working_dir.join(p).display().to_string()
                                                    } else {
                                                        path_key
                                                    }
                                                } else {
                                                    String::new()
                                                };
                                                let norm_key = normalize_path_key(&resolved_path);
                                                let norm_key_log = norm_key.clone();
                                                info!("approval check: tool={} path_key={} norm_key={} cache_size={} cache_contains={}", 
                                                    tc.name, path_key_for_log, norm_key_log, approved_paths.len(), approved_paths.contains(&norm_key));
                                                if !norm_key.is_empty() and approved_paths.contains(&norm_key) {
                                                    info!("auto-approving {} (already approved in this session): {}", tc.name, g.reason);
                                                    (True, None)
                                                } else {
                                                    send(w, &Event::ApprovalRequired {
                                                        id,
                                                        tool_call_id: tc.id.clone(),
                                                        tool_name: tc.name.clone(),
                                                        path: g.path.clone(),
                                                        reason: g.reason.clone(),
                                                    })
                                                    .await?;
                                                    info!("asking approval for {} ({})", tc.name, g.reason);
                                                    # The read loop routes ApprovalResponse
                                                    # here over `approvals`.
                                                    let key = format!("{}:{}", id, tc.id);
                                                    let (tx, rx) = tokio::sync::oneshot::channel();
                                                    approvals.write().await.insert(key.clone(), tx);
                                                    let decision = tokio::time::timeout(
                                                        std::time::Duration::from_secs(300),
                                                        rx,
                                                    )
                                                    .await;
                                                    approvals.write().await.remove(&key);
                                                    let ok = matches!(decision, Ok(Ok(true)));
                                                    if ok and !norm_key.is_empty() {
                                                        let norm_key_clone = norm_key.clone();
                                                        approved_paths.insert(norm_key);
                                                        info!("inserted into approval cache: norm_key={} cache_size={}", norm_key_clone, approved_paths.len());
                                                    }
                                                    # Also extract paths from apply_patch patches to populate cache
                                                    if ok and tc.name == "apply_patch":
                                                        info!("apply_patch approved, extracting paths from patch");
                                                        if let Some(patch) = tc.input.get("patch").and_then(|v| v.as_str()) {
                                                            let extracted = extract_patch_paths(patch);
                                                            info!("extract_patch_paths returned: {:?}", extracted);
                                                            for p in extracted {
                                                                let p_clone = p.clone();
                                                                if !p_clone.is_empty() {
                                                                    approved_paths.insert(p_clone.clone());
                                                                    info!("inserted patch path into cache: {}", p_clone);
                                                                }
                                                            }
                                                        } else {
                                                            info!("apply_patch has no patch field in input");
                                                        }
                                                    }
                                                    (ok, if ok { None } else { Some(g.reason.clone()) })
                                                }
                                            } else {
                                                info!("auto-approving {} in non-interactive mode: {}", tc.name, g.reason);
                                                (True, None)
                                            }
                                        }
                                    };
                                    let result_entry = if approved {
                                        match tool.execute(&tc.input, &ctx).await {'''

content = content.replace(old_loop, new_loop)

# Write the modified content
with open('/home/devops/Projects/jancode/src/server.rs', 'w') as f:
    f.write(content)

print("Done!")