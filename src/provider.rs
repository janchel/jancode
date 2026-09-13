use crate::config::{Config, ProviderConfig};
use crate::protocol::{Event, ToolCall};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    /// Stop sequences. In flattened mode we stop at the tool-request marker so
    /// the model doesn't hallucinate the tool's result (classic ReAct failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolDef {
    #[serde(rename = "type")]
    tool_type: String,
    function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

/// Tool-call wrapper in the API response delta. Only deserialized, never sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApiToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    index: usize,
    /// Groq (and some other providers) require `type: "function"` on each
    /// tool call in the assistant message.
    #[serde(rename = "type", default = "default_tool_call_type")]
    tool_type: String,
    function: ApiFunctionCall,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApiFunctionCall {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Server-sent chunk delta. `content` is optional because some providers emit
/// a leading delta with only `role` set before the first token.
#[derive(Debug, Clone, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiToolCall>>,
    /// Reasoning/thinking tokens streamed by reasoning models (e.g. DeepSeek,
    /// some routers). Captured so the client can surface them as status text.
    #[serde(default)]
    reasoning: Option<String>,
    /// DeepSeek-style reasoning field (some providers use `reasoning_content`
    /// instead of `reasoning`).
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Parsed tool call accumulated across deltas.
#[derive(Debug, Clone)]
struct AccumulatedToolCall {
    id: String,
    name: String,
    args: String,
    /// Ids already seen for this slot, used to detect the start of a new call.
    seen: std::collections::HashSet<String>,
}

/// Send a message to the provider and return a list of events (text deltas,
/// tool calls, and a terminal Done). Supports function-calling when `tools`
/// is provided. `model` is the model to use for this request (session model,
/// CLI override, or the config default).
/// How long the provider may go without sending any body bytes before we give
/// up. This is a stall detector, not a total deadline: a slow-but-alive stream
/// (long context, tool loops) is allowed to run however long it takes.
/// Overridable with `JANCODE_STREAM_STALL_SECS` for diagnosis/tests.
const STREAM_STALL_DEFAULT: std::time::Duration = std::time::Duration::from_secs(120);

fn stream_stall() -> std::time::Duration {
    std::env::var("JANCODE_STREAM_STALL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(STREAM_STALL_DEFAULT)
}

/// Perform the POST and consume the streamed SSE body, reassembling it as a
/// string. Unlike `.text()`, which runs a single total deadline over the whole
/// exchange and trips on long-context generations that stream slowly, this
/// reads chunk-by-chunk and only fails when nothing arrives for `stream_stall`.
/// A stall is surfaced as an `Elapsed` error so the retry logic treats it as
/// transient.
async fn do_chat_request(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    req: &ChatRequest,
) -> Result<String> {
    let mut resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(req)
        .send()
        .await
        .context("sending chat request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = tokio::time::timeout(std::time::Duration::from_secs(30), resp.text())
            .await
            .unwrap_or_else(|_| Ok("(reading error body timed out)".to_string()))
            .unwrap_or_default();
        tracing::error!("provider error {}: {}", status, text);
        anyhow::bail!("provider error {}: {}", status, text);
    }

    // Incremental read: append bytes to a raw buffer, splitting on newlines so
    // the reassembled body has identical line semantics to `.text()`. Each
    // chunk waits up to the stall window; a steady stream never hits the clock.
    let stall = stream_stall();
    let mut buf: Vec<u8> = Vec::new();
    let mut out = String::new();
    // Some routers (e.g. rebelstack) send `keepalive` chunks (`delta:{}`) that
    // reset the per-chunk stall timer. A slow-but-alive upstream router legitimately
    // routes through keepalives for a while before the first real chunk, but a
    // STUCK router can send them forever. Detect keepalive-only chunks and bail
    // once nothing but keepalives have arrived for the whole stall window, so we
    // don't hang forever. This gives a slow router the same generous window as a
    // total stall, but turns an infinite keepalive stream into a bounded error.
    // The budget is the same `stream_stall()` (so `JANCODE_STREAM_STALL_SECS`
    // overrides it too), but measured as keepalive-only time — a real-content
    // chunk also resets it.
    let keepalive_stall = stream_stall();
    let mut first_empty: Option<std::time::Instant> = None;
    loop {
        let chunk = tokio::time::timeout(stall, resp.chunk())
            .await
            .map_err(|elapsed| {
                anyhow::Error::new(elapsed).context(format!(
                    "provider stream stalled: no data for {}s",
                    stall.as_secs()
                ))
            })?;
        let chunk = chunk.context("reading response body")?;
        let Some(bytes) = chunk else { break };
        // Detect keepalive-only chunks: a line that is `data: {...}` with an
        // empty delta and no content/tool_calls. We count them and bail if the
        // provider keeps sending them without progress. Only count a chunk as
        // keepalive if it has NO real content and NO [DONE] marker.
        let line_str = String::from_utf8_lossy(&bytes);
        let is_keepalive = !line_str.contains("[DONE]")
            && line_str.contains("\"delta\":{}")
            && !line_str.contains("\"content\":\"")
            && !line_str.contains("\"tool_calls\"");
        if is_keepalive {
            let now = std::time::Instant::now();
            let start = *first_empty.get_or_insert(now);
            if now.duration_since(start) >= keepalive_stall {
                tracing::error!(
                    "provider stream stalled: only keepalive/empty chunks for {}s without a real response",
                    now.duration_since(start).as_secs()
                );
                anyhow::bail!(
                    "provider stream stalled: only keepalive chunks for {}s without a real response",
                    keepalive_stall.as_secs()
                );
            }
            continue;
        }
        first_empty = None;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            out.push_str(&String::from_utf8_lossy(&line[..line.len() - 1]));
            out.push('\n');
        }
    }
    if !buf.is_empty() {
        out.push_str(&String::from_utf8_lossy(&buf));
    }
    Ok(out)
}

/// True when the failure is a transient transport problem worth retrying once:
/// connection refused/reset, body-read stall, no-data stream stall, or a
/// dropped connection. HTTP status errors and serialization failures are not
/// retried.
fn is_retryable_transport_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        if cause.downcast_ref::<tokio::time::error::Elapsed>().is_some() {
            return true;
        }
        // Keepalive-only stall: transient provider condition (router routed to a
        // slow/overloaded upstream). Worth retrying so a re-route can succeed.
        if cause.to_string().contains("only keepalive chunks") {
            return true;
        }
        cause
            .downcast_ref::<reqwest::Error>()
            .map(|re| re.is_timeout() || re.is_connect() || re.is_body())
            .unwrap_or(false)
    })
}

/// True when the error is an HTTP 429 (rate limit) response.
fn is_rate_limit_error(e: &anyhow::Error) -> bool {
    e.to_string().contains("429")
}

/// Extract the suggested wait time from a 429 rate-limit error message, if the
/// provider includes one (e.g. "Please try again in 13.155s"). Falls back to a
/// default backoff.
fn rate_limit_wait(e: &anyhow::Error) -> std::time::Duration {
    let msg = e.to_string();
    // Look for "in <N>s" or "in <N> seconds" in the message.
    if let Some(idx) = msg.find("in ") {
        let rest = &msg[idx + 3..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(secs) = num.parse::<f64>() {
            if secs > 0.0 {
                return std::time::Duration::from_secs_f64(secs + 1.0);
            }
        }
    }
    std::time::Duration::from_secs(15)
}

/// Normalize tool-call arguments to be robust against models that emit
/// non-standard schemas. Some reasoning models / routers produce arguments
/// that don't match the tool's declared JSON schema (e.g. `file_path` instead
/// of `path`, or a `raw` wrapper around a JSON string). We repair the common
/// cases so the tool can actually run instead of failing and looping.
fn normalize_tool_args(name: &str, input: &Value) -> Value {
    // If the args failed to parse as JSON, they were wrapped in `{"raw": ...}`.
    // Try to recover the inner JSON string and re-parse it.
    if let Some(raw) = input.get("raw").and_then(|v| v.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            return normalize_tool_args(name, &parsed);
        }
        // If the raw string is truncated JSON (unbalanced braces), try to
        // repair it by balancing braces.
        if let Some(repaired) = repair_truncated_json(raw) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&repaired) {
                return normalize_tool_args(name, &parsed);
            }
        }
    }

    // Common schema aliases: map non-standard keys to the canonical ones the
    // tools expect. This handles models that emit `file_path`/`file`/`filename`
    // instead of `path`, etc. We rebuild the object with canonical keys.
    if let Some(obj) = input.as_object() {
        let mut out = serde_json::json!({});
        if let Some(mut out_map) = out.as_object_mut() {
            for (k, v) in obj {
                let canonical = canonical_key(&k);
                // Only set if not already present (don't overwrite a canonical key).
                if !out_map.contains_key(&canonical) {
                    out_map.insert(canonical, v.clone());
                }
            }
        }
        return out;
    }
    input.clone()
}

/// Map a non-standard tool-argument key to the canonical one the tools expect.
fn canonical_key(k: &str) -> String {
    match k {
        "file_path" | "file" | "filename" | "filepath" | "target_path" | "destination" => "path".to_string(),
        "command_line" | "cmd" | "shell_command" => "command".to_string(),
        "query_string" | "search" => "query".to_string(),
        "pattern_str" | "glob_pattern" => "pattern".to_string(),
        "content_str" | "file_content" | "new_content" => "content".to_string(),
        "replacement" => "new_string".to_string(),
        "old_text" | "old_value" => "old_string".to_string(),
        "new_text" | "new_value" => "new_string".to_string(),
        _ => k.to_string(),
    }
}

/// Attempt to repair truncated JSON by balancing braces/brackets. Some models
/// stream tool-call arguments that get cut off (e.g. `{"path": "styles.css"`).
/// We append the missing closing delimiters so the JSON parses.
fn repair_truncated_json(s: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if !stack.is_empty() && stack.last().map(|x| *x).unwrap_or('\0') == c {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
    if stack.is_empty() {
        None
    } else {
        let mut out = s.to_string();
        for c in stack.iter().rev() {
            out.push(*c);
        }
        Some(out)
    }
}

pub async fn send_message(
    cfg: &Config,
    provider_name: Option<&str>,
    model: &str,
    session_messages: &[crate::storage::Message],
    tools: Option<&[crate::tools::ToolDefinition]>,
    memory_context: &str,
    instructions: &str,
    tool_budget: Option<u32>,
) -> Result<Vec<Event>> {
    let provider = crate::config::resolve_provider(cfg, provider_name);
    let api_key = resolve_api_key(cfg, &provider)?;
    let base_url = provider.base_url.trim_end_matches('/');
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(12 * 60 * 60))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut system = if provider.supports_tools {
        "You are a helpful AI coding agent with local filesystem access. You have tools: list_dir, glob, read, agentgrep, bash, write, edit, apply_patch, plan, git, fetch_url, http_request, note, docker, sql.

WORKFLOW (be surgical, not exhaustive):
1. Start with list_dir on '.' to see the project structure.
2. Use glob/agentgrep to FIND files, then read ONLY the specific files you need.
3. Never read all files in a directory — that wastes time and tokens.
4. When the user asks you to modify something, DO IT: read the relevant file, make the change, confirm it. Don't re-explore or re-read files you already have.
5. If the user's message is a greeting or small talk, just respond naturally — don't start exploring the filesystem.

WORKSPACE (CRITICAL):
Your working directory is your confined workspace. Use relative paths (e.g.
`styles.css`, `src/main.rs`) — never absolute paths. Do NOT prepend the absolute
path, and do NOT `cd /abs/path/...` to reach files that are already in your own
workspace; your tools already run inside it. Only touch a location OUTSIDE the
workspace if it's genuinely needed — it will require the user's approval.

APPROVALS (CRITICAL):
Some operations (write/apply_patch, bash mutations, git, docker, http non-GET)
require the user to approve them. If a tool result says \"APPROVAL_DENIED\" or the
user denies an operation, STOP retrying that operation. Do NOT keep issuing the
same or similar gated operations hoping one gets approved — that wastes the whole
tool budget and looks like a loop. Instead:
- If a full-file write was denied, switch to small targeted `edit` calls that
  only change the exact lines needed (these are usually approved).
- If edits keep failing or being denied, give your final answer and explain what
  you could not change and why.

The user's time and tokens are limited. Be concise and direct.".to_string()
    } else {
        // No tool calling: the model answers directly. Don't mention tools so
        // it doesn't try to call them (which would just loop).
        "You are a helpful AI assistant. You do NOT have access to any tools or the filesystem. Answer the user's questions directly and concisely based on your knowledge. If you don't know something, say so. Do not mention tools, files, or directories — just answer the question.".to_string()
    };
    if !instructions.is_empty() {
        system.push_str("\n\n# Project instructions (AGENTS.md)\n");
        system.push_str("Follow the project instructions below. They come from the repository's AGENTS.md/CLAUDE.md files and describe the project's conventions, build/test commands, and operating rules. They take precedence over generic guidance, but never override the user's direct request.\n");
        system.push_str(instructions);
    }
    if !memory_context.is_empty() {
        system.push_str("\n\nProject memory (facts the user has told you before; trust them unless they conflict with what you see):\n");
        system.push_str(memory_context);
    }

    // In flattened mode the provider can't receive structured `tool_calls`, so
    // the model must emit tool calls as text markers that we parse. Tell it
    // exactly how to do that, otherwise it just asks the user to run tools.
    // Only relevant when tools are enabled.
    if provider.supports_tools && provider.tool_call_style == "flattened" {
        system.push_str(
            "\n\nTOOL CALLING FORMAT (CRITICAL):\n\
             Call tools by emitting a JSON block:\n\
             [tool_request]\n\
             {\"name\": \"<tool_name>\", \"arguments\": {<json args>}}\n\
             [END_TOOL_REQUEST]\n\
             Then STOP — do not write the tool's result yourself; the system\n\
             executes it and returns the real result next.\n\
             \n\
             Example:\n\
             User: what files are here?\n\
             Assistant: [tool_request]\n\
             {\"name\": \"list_dir\", \"arguments\": {\"path\": \".\"}}\n\
             [END_TOOL_REQUEST]\n\
             \n\
             User: [tool result]: index.html\nscript.js\nstyles.css\n\
             Assistant: The directory contains index.html, script.js, and styles.css.\n\
             \n\
             Use EXACTLY the paths returned by list_dir. When editing, use the\n\
             same path you read from. Once you have what you need, make the\n\
             change and give your final answer in plain text.\n",
        );
    }

    // Budget signaling: tell the model how many tool calls it has left so it
    // wraps up instead of exploring forever. Only meaningful when tools are on.
    if provider.supports_tools {
        if let Some(budget) = tool_budget {
            system.push_str(&format!(
                "\n\nTOOL BUDGET (CRITICAL):\n\
                 You have approximately {} tool call(s) remaining for this task.\n\
                 Use them wisely: prefer reading the specific file you need over\n\
                 broad exploration. Once you have enough information, make the\n\
                 change and give your final answer. Do not waste calls re-reading\n\
                 the same file or re-listing directories you already saw.\n",
                budget
            ));
        }
    }

    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: Some(system),
        tool_calls: None,
        tool_call_id: None,
    });

    // Some providers (e.g. Gemma via LM Studio) reject the OpenAI
    // `tool_calls`/`tool` message roles in the request history. When
    // `tool_call_style = "flattened"`, we convert tool-call history into plain
    // user/assistant messages so those providers accept it.
    //
    // `"auto"` starts with the OpenAI native format but ALSO parses text-based
    // tool calls as a fallback, so a model that emits `[tool_request]` markers
    // instead of structured `tool_calls` still works.
    //
    // We deliberately do NOT flatten on `auto` even when the history contains
    // `tool`/`tool_calls` roles. Models served through `auto` combos (e.g.
    // rebelstack) handle native `tool_calls` fine, and forcing the flattened
    // text format on subsequent turns makes the combo router stall / route to
    // an overloaded upstream. Native format round-trips reliably. Only
    // `tool_call_style = "flattened"` (for providers like LM Studio that reject
    // tool roles) triggers the flattened text encoding.
    let has_tool_history = session_messages.iter().any(|m| m.role == "tool" || m.tool_calls.is_some());
    let flattened = provider.tool_call_style == "flattened";
    let auto_style = provider.tool_call_style == "auto";

    if flattened {
        for m in session_messages {
            if m.role == "tool" {
                // A tool result becomes a user message describing the output.
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(format!("[tool result]: {}", m.content)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            } else if let Some(tcs) = &m.tool_calls {
                // An assistant message that made tool calls: describe the calls
                // in the assistant text (no `tool_calls` field).
                let calls_desc = tcs
                    .iter()
                    .map(|tc| format!("{} {}", tc.name, tc.arguments))
                    .collect::<Vec<_>>()
                    .join("; ");
                let text = if m.content.is_empty() {
                    format!("[calling tools: {}]", calls_desc)
                } else {
                    format!("{}\n[calling tools: {}]", m.content, calls_desc)
                };
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                });
            } else {
                messages.push(ChatMessage {
                    role: m.role.clone(),
                    content: Some(m.content.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
    } else {
        messages.extend(
            session_messages
                .iter()
                .map(|m| ChatMessage {
                    role: m.role.clone(),
                    // Provider compatibility: an assistant message that carries
                    // `tool_calls` must have `content` as null/omitted (not an
                    // empty string) — LM Studio's Gemma rejects `content:""` on a
                    // tool-call message. Tool-result and regular messages keep a
                    // string content.
                    content: if m.tool_calls.is_some() {
                        None
                    } else {
                        Some(m.content.clone())
                    },
                    tool_calls: m.tool_calls.as_ref().map(|tcs| {
                        tcs.iter()
                            .enumerate()
                            .map(|(i, tc)| ApiToolCall {
                                id: Some(tc.id.clone()),
                                index: i,
                                tool_type: "function".to_string(),
                                function: ApiFunctionCall {
                                    name: Some(tc.name.clone()),
                                    arguments: Some(tc.arguments.clone()),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: m.tool_call_id.clone(),
                }),
        );
    }

    let tool_defs = tools.map(|tds| {
        tds.iter()
            .map(|td| ToolDef {
                tool_type: "function".to_string(),
                function: ToolFunction {
                    name: td.name.clone(),
                    description: td.description.clone(),
                    parameters: td.parameters.clone(),
                },
            })
            .collect::<Vec<_>>()
    });

    let tool_choice = if tool_defs.is_some() {
        Some(serde_json::json!("auto"))
    } else {
        Some(serde_json::json!("none"))
    };

    // Optional context-window trimming: if `context_window` is set, drop the
    // oldest messages (keeping the system prompt and the most recent turns)
    // so the request stays within the model's context budget. This reduces
    // token usage and rate-limit pressure on providers like Groq.
    if provider.context_window > 0 && messages.len() > 4 {
        // Rough token estimate: ~4 chars per token.
        let budget = provider.context_window;
        let mut total: u64 = 0;
        let mut keep_from = messages.len();
        // Walk backwards from the newest message, accumulating until we hit
        // the budget. Always keep at least the system prompt + last 2 turns.
        let min_keep = 3; // system + user + assistant
        for i in (min_keep..messages.len()).rev() {
            let est = (messages[i].content.as_deref().map_or(0, |c| c.len()) / 4) as u64;
            if total + est > budget {
                break;
            }
            total += est;
            keep_from = i;
        }
        if keep_from > min_keep {
            tracing::info!(
                "context window {}: trimming {} oldest messages (keeping {}..{})",
                budget,
                keep_from - min_keep,
                keep_from,
                messages.len(),
            );
            let mut trimmed = Vec::new();
            trimmed.push(messages[0].clone()); // system prompt
            for i in keep_from..messages.len() {
                trimmed.push(messages[i].clone());
            }
            messages = trimmed;
        }
    }

    let req = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        tools: tool_defs,
        tool_choice,
        max_tokens: if provider.max_tokens > 0 {
            Some(provider.max_tokens)
        } else {
            None
        },
        // In flattened mode, stop generation right after the model emits the
        // tool-request marker so it can't hallucinate the tool's result. The
        // server feeds the real result back in the next turn.
        stop: if flattened {
            Some(vec!["[END_TOOL_REQUEST]".to_string()])
        } else {
            None
        },
    };

    let url = format!("{}/chat/completions", base_url);
    tracing::info!("provider request: model={}, messages={}, tools={}", req.model, req.messages.len(), req.tools.as_ref().map_or(0, |t| t.len()));
    if let Some(ref tool_list) = req.tools {
        for td in tool_list {
            tracing::info!("  tool: {} - {}", td.function.name, td.function.description.chars().take(60).collect::<String>());
        }
    }
    // Debug: dump the exact JSON payload so we can see what the provider rejects.
    if let Ok(json) = serde_json::to_string(&req) {
        tracing::debug!("provider request body: {}", json);
    }
    let mut events = Vec::new();
    let mut full = String::new();
    // Accumulate reasoning/thinking tokens and emit them as ONE Status event
    // after the stream, so the client shows a single "[thinking] ..." line
    // instead of one line per token.
    let mut reasoning_buf = String::new();

    // Accumulate tool calls across deltas; emit them as a single ToolCall event
    // when the turn finishes.
    let mut pending: Vec<AccumulatedToolCall> = Vec::new();

    // Retry on transient transport problems (timeout, dropped connection,
    // body-read stall) and on 429 rate-limit responses (with backoff). The
    // whole request is re-issued; this is safe because the provider call is
    // just a generation, not a mutation. Other HTTP errors (4xx/5xx) are
    // surfaced as-is.
    //
    // For 429s we only retry SHORT waits (e.g. per-minute token limits). A
    // long wait (e.g. "tokens per day" with a 29-minute reset) is surfaced
    // immediately so the user can switch models instead of hanging for the
    // retry budget.
    const MAX_RATE_LIMIT_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
    let body = {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match do_chat_request(&client, &url, &api_key, &req).await {
                Ok(body) => break body,
                Err(e) if attempt <= 3 && is_retryable_transport_error(&e) => {
                    tracing::warn!("provider transport failure: {:?}; retrying (attempt {})", e, attempt);
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                    continue;
                }
                Err(e) if attempt <= 3 && is_rate_limit_error(&e) => {
                    let wait = rate_limit_wait(&e);
                    if wait > MAX_RATE_LIMIT_RETRY_WAIT {
                        // Long wait (daily quota, etc.) — don't hang; surface
                        // the error so the user can switch models.
                        tracing::error!(
                            "provider rate limited with long wait ({}s); not retrying — switch models",
                            wait.as_secs()
                        );
                        return Err(e);
                    }
                    tracing::warn!("provider rate limited; retrying in {}s (attempt {})", wait.as_secs(), attempt);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    };
    tracing::info!("provider response: {} lines, {} bytes", body.lines().count(), body.len());
    tracing::debug!("provider response body: {}", body);
    for line in body.lines() {
        process_line(line, &mut events, &mut full, &mut pending, &mut reasoning_buf);
    }

    // Emit the accumulated reasoning as a single Status event (if any).
    if !reasoning_buf.is_empty() {
        events.push(Event::Status {
            id: None,
            message: format!("thinking: {}", reasoning_buf.trim()),
        });
    }

    // Flush any accumulated tool calls into a ToolCall event.
    let had_structured_calls = !pending.is_empty();
    if had_structured_calls {
        let calls: Vec<ToolCall> = pending
            .into_iter()
            .map(|a| {
                let name = a.name.clone();
                let input: Value = serde_json::from_str(&a.args).unwrap_or_else(|_| {
                    serde_json::json!({ "raw": a.args.clone() })
                });
                let norm = normalize_tool_args(&name, &input);
                ToolCall {
                    id: a.id,
                    name,
                    input: norm,
                }
            })
            .collect();
        events.push(Event::ToolCall { id: None, calls });
    }

    // In flattened mode the model writes tool calls as text
    // (`[calling tools: name {"arg":...}]`) instead of structured `tool_calls`.
    // If no structured tool calls were emitted but the text contains the
    // marker, parse it and emit a ToolCall event so the tools actually run.
    // In "auto" mode we also parse text markers as a fallback, since some
    // models emit them even when structured tool calling is advertised.
    if (flattened || auto_style) && !had_structured_calls {
        if let Some(calls) = parse_flattened_tool_calls(&full) {
            events.push(Event::ToolCall { id: None, calls });
        }
    }

    // Ensure a terminal Done event.
    if !events.iter().any(|ev| matches!(ev, Event::Done { .. })) {
        events.push(Event::Done { id: Some(0) });
    }
    if events.is_empty() {
        events.push(Event::TextDelta { id: None, text: full });
        events.push(Event::Done { id: Some(0) });
    }
    Ok(events)
}

/// Parse tool calls written as text in flattened mode. Handles two formats the
/// model may emit:
///   1. `[calling tools: read {"path":"styles.css"}]` (compact, `;`-separated)
///   2. `[tool_request] {json} [END_TOOL_REQUEST]` (JSON block)
/// Returns `None` if no tool-call marker is present.
fn parse_flattened_tool_calls(text: &str) -> Option<Vec<ToolCall>> {
    let mut calls = Vec::new();

    // Format 2: `[tool_request] ... [END_TOOL_REQUEST]` JSON blocks.
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find("[tool_request]") {
        let abs_start = search_from + start + "[tool_request]".len();
        let rest = &text[abs_start..];
        let end = rest.find("[END_TOOL_REQUEST]")?;
        let body = rest[..end].trim();
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = v.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({}));
            if !name.is_empty() {
                calls.push(ToolCall {
                    id: format!("text-{}", calls.len()),
                    name,
                    input: args,
                });
            }
        }
        search_from = abs_start + end + "[END_TOOL_REQUEST]".len();
    }

    // Format 1: `[calling tools: ...]` compact marker.
    if let Some(start) = text.find("[calling tools:") {
        let rest = &text[start + "[calling tools:".len()..];
        if let Some(end) = rest.find(']') {
            let body = rest[..end].trim();
            if !body.is_empty() {
                // If the body is (or starts with) a JSON object/array, the
                // call args may legitimately contain `;` (e.g. CSS inside an
                // edit call), so we must NOT split on `;`. Parse it as one
                // tool call instead.
                //
                // Two shapes:
                //   a) `edit {"path":"x","content":"a;b"}`  (name + JSON args)
                //   b) `edit` with no args, or `name arg` (plain)
                let first_token_end = body
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| i)
                    .unwrap_or(body.len());
                let (name_part, args_part) = body.split_at(first_token_end);
                let name = name_part.trim().to_string();
                let args_part = args_part.trim();
                if !name.is_empty() {
                    // Try to parse the argument portion as JSON.
                    let parsed_args: Option<Value> = if args_part.is_empty() {
                        Some(serde_json::json!({}))
                    } else if args_part.starts_with('{') {
                        // One JSON object → one tool call (even if it contains
                        // `;` inside string values).
                        serde_json::from_str::<Value>(args_part).ok()
                    } else {
                        // Mixed / multiple plain calls — split on `;` but only
                        // if not inside a JSON object. This is the legacy path
                        // for `list_dir {}; read {"path":"x"}` style markers.
                        None
                    };
                    match parsed_args {
                        Some(input) => {
                            calls.push(ToolCall {
                                id: format!("text-{}", calls.len()),
                                name: name.clone(),
                                input,
                            });
                        }
                        None => {
                            // Legacy multi-call: split on `;`, but only split
                            // each segment at its own JSON args.
                            for part in body.split(';') {
                                let part = part.trim();
                                if part.is_empty() {
                                    continue;
                                }
                                let mut it = part.splitn(2, char::is_whitespace);
                                let n = it.next().unwrap_or("").trim().to_string();
                                let args_str = it.next().unwrap_or("{}").trim();
                                if n.is_empty() {
                                    continue;
                                }
                                let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| {
                                    serde_json::json!({ "raw": args_str })
                                });
                                calls.push(ToolCall {
                                    id: format!("text-{}", calls.len()),
                                    name: n,
                                    input,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

fn process_line(
    line: &str,
    events: &mut Vec<Event>,
    full: &mut String,
    pending: &mut Vec<AccumulatedToolCall>,
    reasoning_buf: &mut String,
) {
    let line = line.trim();
    if !line.starts_with("data: ") {
        return;
    }
    let data = &line[6..];
    if data == "[DONE]" {
        events.push(Event::Done { id: Some(0) });
        return;
    }
    let chunk: StreamChunk = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "failed to parse SSE chunk {:?}: {}",
                data.chars().take(80).collect::<String>(),
                e
            );
            return;
        }
    };
    if let Some(choice) = chunk.choices.first() {
        // Text delta
        if let Some(text) = &choice.delta.content {
            if !text.is_empty() {
                full.push_str(text);
                events.push(Event::TextDelta { id: None, text: text.clone() });
            }
        }

        // Reasoning/thinking tokens (reasoning models). Accumulate them into the
        // buffer; they're emitted as a single Status event after the stream.
        // Handles both `reasoning` (OpenAI-style) and `reasoning_content`
        // (DeepSeek-style) field names.
        let reasoning = choice.delta.reasoning
            .as_ref()
            .or_else(|| choice.delta.reasoning_content.as_ref());
        if let Some(r) = reasoning {
            if !r.is_empty() {
                reasoning_buf.push_str(r);
            }
        }

        // Tool-call delta (OpenAI streams tool_calls as an array with deltas).
        // Different providers format these differently, so we accumulate by
        // index and treat a fresh id as the start of a new call. Argument
        // fragments are appended whether or not the chunk carries an id, which
        // handles both OpenAI (id only on the first chunk) and Claude-style
        // routers (id on every chunk).
        if let Some(tcs) = &choice.delta.tool_calls {
            for tc in tcs {
                // Ensure the slot for this index exists.
                while pending.len() <= tc.index {
                    pending.push(AccumulatedToolCall {
                        id: String::new(),
                        name: String::new(),
                        args: String::new(),
                        seen: std::collections::HashSet::new(),
                    });
                }
                let slot = &mut pending[tc.index];
                // A fresh id means the start of a new tool call.
                if let Some(id) = &tc.id {
                    if !id.is_empty() && !slot.seen.contains(id) {
                        slot.seen.insert(id.clone());
                        slot.id = id.clone();
                        slot.args.clear();
                    }
                }
                if let Some(name) = &tc.function.name {
                    if !name.is_empty() {
                        slot.name = name.clone();
                    }
                }
                if let Some(args) = &tc.function.arguments {
                    if !args.is_empty() {
                        slot.args.push_str(args);
                    }
                }
            }
        }

        if choice.finish_reason.is_some() {
            events.push(Event::Done { id: Some(0) });
        }
    }
}

/// Fetch the model catalog from the provider. Prefers the explicit `models`
/// list in config; otherwise queries the OpenAI-compatible `GET /models`
/// endpoint. Used by the `/model` interactive picker.
pub async fn list_models(cfg: &Config, provider_name: Option<&str>) -> Result<Vec<String>> {
    let provider = crate::config::resolve_provider(cfg, provider_name);
    if !provider.models.is_empty() {
        return Ok(provider.models.clone());
    }
    let api_key = resolve_api_key(cfg, &provider)?;
    let base_url = provider.base_url.trim_end_matches('/');
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    #[derive(Deserialize)]
    struct ModelsResponse {
        #[serde(default)]
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
    }

    let resp = client
        .get(format!("{}/models", base_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("listing provider models")?;
    if !resp.status().is_success() {
        anyhow::bail!("provider /models returned {}", resp.status());
    }
    let parsed: ModelsResponse = resp.json().await.context("parsing /models response")?;
    Ok(parsed.data.into_iter().map(|m| m.id).collect())
}

fn resolve_api_key(cfg: &Config, provider: &ProviderConfig) -> Result<String> {
    // 1. Inline key in config (easiest for single-user setups)
    if let Some(ref key) = provider.api_key {
        if !key.trim().is_empty() {
            return Ok(key.clone());
        }
    }
    // 2. Named environment variable from config
    if let Some(ref env_name) = provider.api_key_env {
        if let Ok(val) = std::env::var(env_name) {
            if !val.trim().is_empty() {
                return Ok(val);
            }
        }
    }
    // 3. Default OPENAI_API_KEY
    if let Ok(val) = std::env::var("OPENAI_API_KEY") {
        if !val.trim().is_empty() {
            return Ok(val);
        }
    }
    anyhow::bail!(
        "no API key found; set provider.api_key in config.toml, or set {} env var, or set OPENAI_API_KEY",
        provider.api_key_env.as_deref().unwrap_or("api_key_env")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flattened_tool_calls() {
        // Single call with JSON args.
        let calls = parse_flattened_tool_calls(
            "Let me read the file.\n[calling tools: read {\"path\":\"styles.css\"}]",
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].input["path"], "styles.css");

        // Multiple calls separated by `;`.
        let calls = parse_flattened_tool_calls(
            "[calling tools: list_dir {}; read {\"path\":\"index.html\"}]",
        )
        .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "list_dir");
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].input["path"], "index.html");

        // No marker -> None.
        assert!(parse_flattened_tool_calls("just some text").is_none());
        // Empty marker -> None.
        assert!(parse_flattened_tool_calls("[calling tools: ]").is_none());

        // JSON block format: `[tool_request] {json} [END_TOOL_REQUEST]`.
        let calls = parse_flattened_tool_calls(
            "Let me read the file.\n[tool_request]\n{\"name\":\"read\",\"arguments\":{\"path\":\"styles.css\"}}\n[END_TOOL_REQUEST]",
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].input["path"], "styles.css");

        // Multiple JSON blocks.
        let calls = parse_flattened_tool_calls(
            "[tool_request]\n{\"name\":\"list_dir\",\"arguments\":{}}\n[END_TOOL_REQUEST]\n[tool_request]\n{\"name\":\"read\",\"arguments\":{\"path\":\"index.html\"}}\n[END_TOOL_REQUEST]",
        )
        .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "list_dir");
        assert_eq!(calls[1].name, "read");

        // CRITICAL: args containing `;` (e.g. CSS/new_string) must NOT be
        // split on `;`. The whole JSON object is one tool call.
        let calls = parse_flattened_tool_calls(
            "[calling tools: edit {\"path\":\"styles.css\",\"new_string\":\"body { color: #fff; background: rgba(26,60,52,0.98); }\",\"old_string\":\"body { color: #000; }\"}]",
        )
        .unwrap();
        assert_eq!(calls.len(), 1, "should be exactly ONE edit call, got {}", calls.len());
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].input["new_string"], "body { color: #fff; background: rgba(26,60,52,0.98); }");
    }

    #[test]
    fn rate_limit_detection() {
        let e = anyhow::anyhow!("provider error 429 Too Many Requests: rate limit");
        assert!(is_rate_limit_error(&e));
        let e2 = anyhow::anyhow!("provider error 500 Internal Server Error");
        assert!(!is_rate_limit_error(&e2));
    }

    #[test]
    fn rate_limit_wait_parsing() {
        // Parses the "in 13.155s" hint from the error message.
        let e = anyhow::anyhow!("Please try again in 13.155s. Need more tokens?");
        let wait = rate_limit_wait(&e);
        assert!(wait.as_secs() >= 14, "expected ~14s, got {}", wait.as_secs());
        // Falls back to 15s when no hint is present.
        let e2 = anyhow::anyhow!("provider error 429 Too Many Requests");
        assert_eq!(rate_limit_wait(&e2).as_secs(), 15);
    }

    #[test]
    fn normalizes_tool_args() {
        // `file_path` alias -> `path`.
        let v = normalize_tool_args("read", &serde_json::json!({"file_path": "styles.css"}));
        assert_eq!(v["path"], "styles.css");
        // `raw` wrapper around a JSON string is unwrapped.
        let v = normalize_tool_args("write", &serde_json::json!({"raw": "{\"path\": \"a.css\", \"content\": \"x\"}"}));
        assert_eq!(v["path"], "a.css");
        assert_eq!(v["content"], "x");
        // Truncated JSON in `raw` is repaired (unbalanced brace).
        let v = normalize_tool_args("write", &serde_json::json!({"raw": "{\"path\": \"a.css\""}));
        assert_eq!(v["path"], "a.css");
        // Canonical keys are preserved.
        let v = normalize_tool_args("read", &serde_json::json!({"path": "b.css"}));
        assert_eq!(v["path"], "b.css");
    }

    #[test]
    fn repairs_truncated_json() {
        assert_eq!(repair_truncated_json("{\"path\": \"a.css\"").unwrap(), "{\"path\": \"a.css\"}");
        assert_eq!(repair_truncated_json("{\"a\": [1, 2").unwrap(), "{\"a\": [1, 2]}");
        // Balanced JSON -> None (nothing to repair).
        assert!(repair_truncated_json("{\"a\": 1}").is_none());
    }
}
