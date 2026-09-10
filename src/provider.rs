use crate::config::Config;
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
    function: ApiFunctionCall,
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
        cause
            .downcast_ref::<reqwest::Error>()
            .map(|re| re.is_timeout() || re.is_connect() || re.is_body())
            .unwrap_or(false)
    })
}

pub async fn send_message(
    cfg: &Config,
    model: &str,
    session_messages: &[crate::storage::Message],
    tools: Option<&[crate::tools::ToolDefinition]>,
    memory_context: &str,
    instructions: &str,
) -> Result<Vec<Event>> {
    let api_key = resolve_api_key(cfg)?;
    let base_url = cfg.provider.base_url.trim_end_matches('/');
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(12 * 60 * 60))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut system = "You are a helpful AI coding agent running with full local filesystem access on the user's machine. You have tools to explore the codebase: list_dir (list a directory), glob (find files by pattern), read (read file contents), agentgrep (search file contents), bash (run shell commands), write (create/overwrite files), and edit (replace text in a file). When the user asks you to analyze or inspect a project, USE these tools to explore the working directory yourself before responding — do not ask the user for file paths or tell them you lack access. Start by calling list_dir on '.' or the current directory to discover the structure.".to_string();
    if !instructions.is_empty() {
        system.push_str("\n\n# Project instructions (AGENTS.md)\n");
        system.push_str("Follow the project instructions below. They come from the repository's AGENTS.md/CLAUDE.md files and describe the project's conventions, build/test commands, and operating rules. They take precedence over generic guidance, but never override the user's direct request.\n");
        system.push_str(instructions);
    }
    if !memory_context.is_empty() {
        system.push_str("\n\nProject memory (facts the user has told you before; trust them unless they conflict with what you see):\n");
        system.push_str(memory_context);
    }

    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: Some(system),
        tool_calls: None,
        tool_call_id: None,
    });
    messages.extend(
        session_messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: if m.content.is_empty() { None } else { Some(m.content.clone()) },
                tool_calls: m.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .enumerate()
                        .map(|(i, tc)| ApiToolCall {
                            id: Some(tc.id.clone()),
                            index: i,
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

    let req = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        tools: tool_defs,
        tool_choice,
    };

    let url = format!("{}/chat/completions", base_url);
    tracing::info!("provider request: model={}, messages={}, tools={}", req.model, req.messages.len(), req.tools.as_ref().map_or(0, |t| t.len()));
    if let Some(ref tool_list) = req.tools {
        for td in tool_list {
            tracing::info!("  tool: {} - {}", td.function.name, td.function.description.chars().take(60).collect::<String>());
        }
    }
    let mut events = Vec::new();
    let mut full = String::new();

    // Accumulate tool calls across deltas; emit them as a single ToolCall event
    // when the turn finishes.
    let mut pending: Vec<AccumulatedToolCall> = Vec::new();

    // Retry once when the failure is a transient transport problem (timeout,
    // dropped connection, or body-read stall). The whole request is re-issued;
    // this is safe because the provider call is just a generation, not a
    // mutation. Non-transport errors (HTTP 4xx/5xx etc.) are surfaced as-is.
    let body = {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match do_chat_request(&client, &url, &api_key, &req).await {
                Ok(body) => break body,
                Err(e) if attempt == 1 && is_retryable_transport_error(&e) => {
                    tracing::warn!("provider transport failure: {:?}; retrying once", e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    };
    tracing::info!("provider response: {} lines, {} bytes", body.lines().count(), body.len());
    tracing::debug!("provider response body: {}", body);
    for line in body.lines() {
        process_line(line, &mut events, &mut full, &mut pending);
    }

    // Flush any accumulated tool calls into a ToolCall event.
    if !pending.is_empty() {
        let calls: Vec<ToolCall> = pending
            .into_iter()
            .map(|a| {
                let input: Value = serde_json::from_str(&a.args).unwrap_or_else(|_| {
                    serde_json::json!({ "raw": a.args.clone() })
                });
                ToolCall {
                    id: a.id,
                    name: a.name,
                    input,
                }
            })
            .collect();
        events.push(Event::ToolCall { id: None, calls });
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

fn process_line(
    line: &str,
    events: &mut Vec<Event>,
    full: &mut String,
    pending: &mut Vec<AccumulatedToolCall>,
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
pub async fn list_models(cfg: &Config) -> Result<Vec<String>> {
    if !cfg.provider.models.is_empty() {
        return Ok(cfg.provider.models.clone());
    }
    let api_key = resolve_api_key(cfg)?;
    let base_url = cfg.provider.base_url.trim_end_matches('/');
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

fn resolve_api_key(cfg: &Config) -> Result<String> {
    // 1. Inline key in config (easiest for single-user setups)
    if let Some(ref key) = cfg.provider.api_key {
        if !key.trim().is_empty() {
            return Ok(key.clone());
        }
    }
    // 2. Named environment variable from config
    if let Some(ref env_name) = cfg.provider.api_key_env {
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
        cfg.provider.api_key_env.as_deref().unwrap_or("api_key_env")
    );
}
