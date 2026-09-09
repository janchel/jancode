use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Info about a single tool exposed by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Read-only snapshot of one configured MCP server after probing it: whether
/// it connected, and which tools it exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerProbe {
    pub name: String,
    pub url: String,
    pub transport: String,
    pub ok: bool,
    pub error: Option<String>,
    pub tools: Vec<McpToolInfo>,
}

/// Minimal JSON-RPC 2.0 client for the MCP **HTTP streamable** transport.
/// Opened per request turn and shared (via a mutex) across all tool calls in
/// that turn so a single MCP session is reused.
pub struct McpClient {
    url: String,
    http: reqwest::Client,
    session_id: Option<String>,
    next_id: u64,
    bearer_token: Option<String>,
}

impl McpClient {
    pub fn new(url: &str) -> Self {
        Self::with_bearer(url, None)
    }

    /// Create a client that sends `Authorization: Bearer <token>` on every
    /// request (e.g. from a `bearer_env` config value).
    pub fn with_bearer(url: &str, bearer_token: Option<String>) -> Self {
        Self {
            url: url.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            session_id: None,
            next_id: 0,
            bearer_token,
        }
    }

    /// Perform the MCP `initialize` handshake and the `notifications/initialized`
    /// follow-up. Safe to call once per connection.
    pub async fn initialize(&mut self) -> Result<()> {
        let res: Value = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "jancode", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await
            .with_context(|| format!("MCP initialize failed for {}", self.url))?;
        tracing::info!(
            "MCP {} initialized: {:?}",
            self.url,
            res.get("serverInfo").unwrap_or(&serde_json::Value::Null)
        );
        // Notify the server that initialization is complete (fire-and-forget).
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let _ = self.raw_post(&notification).await;
        Ok(())
    }

    /// List the tools exposed by the server.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolInfo>> {
        let res: Value = self
            .rpc("tools/list", json!({}))
            .await
            .with_context(|| format!("MCP tools/list failed for {}", self.url))?;
        let mut tools = Vec::new();
        for t in res
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            tools.push(McpToolInfo {
                name: t.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                input_schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({
                    "type": "object",
                    "properties": {}
                })),
            });
        }
        Ok(tools)
    }

    /// Invoke a tool and return its output as a plain string.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String> {
        let res: Value = self
            .rpc(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments
                }),
            )
            .await
            .with_context(|| format!("MCP tools/call failed for {}", self.url))?;
        let is_error = res.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut text = String::new();
        for c in res.get("content").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                text.push_str(t);
                text.push('\n');
            }
        }
        if text.trim().is_empty() {
            text = res.to_string();
        }
        if is_error {
            anyhow::bail!("MCP tool {} error: {}", name, text.trim());
        }
        Ok(text)
    }

    /// Send a JSON-RPC request and extract its `result`.
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let data = self.raw_post(&body).await?;
        let envelope: Value = parse_envelope(&data, id)?;
        if let Some(err) = envelope.get("error") {
            anyhow::bail!("MCP {} {}: {}", method, self.url, err);
        }
        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    /// POST a JSON body and return the raw response text, capturing the MCP
    /// session id if the server issues one.
    async fn raw_post(&mut self, body: &Value) -> Result<String> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(ref token) = self.bearer_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        if let Some(ref sid) = self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req.json(body).send().await.context("sending MCP request")?;
        if let Some(sid) = resp.headers().get("Mcp-Session-Id") {
            if let Ok(s) = sid.to_str() {
                self.session_id = Some(s.to_string());
            }
        }
        if let Some(ver) = resp.headers().get("Mcp-Protocol-Version") {
            let _ = ver;
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("MCP {} HTTP {}: {}", self.url, status, text);
        }
        Ok(text)
    }
}

/// Parse an MCP response body into a JSON-RPC envelope for `id`. Handles both
/// a plain `application/json` response and an SSE (`text/event-stream`) body.
fn parse_envelope(data: &str, id: u64) -> Result<Value> {
    if data.trim_start().starts_with('{') {
        return Ok(serde_json::from_str(data).context("parsing JSON-RPC envelope")?);
    }
    // SSE: `event: message\n data: {...}\n\nevent: message\ndata: {...}`.
    let mut last: Option<Value> = None;
    for block in data.split("\n\n") {
        let mut event_name = "message";
        let mut payload = String::new();
        for line in block.lines().skip_while(|l| l.is_empty()) {
            let (key, rest) = line.split_once(':').unwrap_or(("", ""));
            let val = rest.trim();
            match key {
                "event" => event_name = val,
                "data" => {
                    if !payload.is_empty() {
                        payload.push('\n');
                    }
                    payload.push_str(val);
                }
                _ => {}
            }
        }
        if event_name == "message" && !payload.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                    return Ok(v);
                }
                last = Some(v);
            }
        }
    }
    last.ok_or_else(|| anyhow::anyhow!("no JSON-RPC envelope with id {} in MCP response", id))
}

/// Tool adapter that exposes a remote MCP tool through jancode's `Tool` trait.
pub struct McpTool {
    pub client: std::sync::Arc<tokio::sync::Mutex<McpClient>>,
    pub info: McpToolInfo,
}

#[async_trait]
impl crate::tools::Tool for McpTool {
    fn name(&self) -> &str {
        &self.info.name
    }

    fn description(&self) -> &str {
        &self.info.description
    }

    fn parameters(&self) -> Value {
        self.info.input_schema.clone()
    }

    async fn execute(&self, input: &Value, _ctx: &crate::tools::ToolContext) -> Result<String> {
        let mut client = self.client.lock().await;
        client.call_tool(&self.info.name, input).await
    }
}

/// Connect to every configured MCP server and return their tools, merged as
/// jancode `Tool`s. Failed servers are logged and skipped so the rest of the
/// conversation still works.
pub async fn load_tools(cfg: &crate::config::Config) -> Vec<Box<dyn crate::tools::Tool>> {
    let mut out: Vec<Box<dyn crate::tools::Tool>> = Vec::new();
    for svc in &cfg.mcp.servers {
        let token = svc
            .bearer_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok());
        let client = std::sync::Arc::new(tokio::sync::Mutex::new(McpClient::with_bearer(
            &svc.url,
            token,
        )));
        let info = match (async {
            let mut c = client.lock().await;
            c.initialize().await?;
            c.list_tools().await
        })
        .await
        {
            Ok(info) => info,
            Err(e) => {
                tracing::error!("MCP server {} ({}) skipped: {:?}", svc.name, svc.url, e);
                continue;
            }
        };
        tracing::info!("MCP server {} exposed {} tools", svc.name, info.len());
        for t in info {
            out.push(Box::new(McpTool {
                client: client.clone(),
                info: t,
            }));
        }
    }
    out
}

/// Read-only probe of every configured MCP server: connect, initialize, and
/// list its tools. Never executes tools. Used by `/mcp_tools` and `/mcp_status`.
pub async fn probe(cfg: &crate::config::Config) -> Vec<McpServerProbe> {
    let mut out = Vec::new();
    for svc in &cfg.mcp.servers {
        let token = svc
            .bearer_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok());
        let mut client = McpClient::with_bearer(&svc.url, token);
        let (ok, error, tools) = match (async {
            client.initialize().await?;
            client.list_tools().await
        })
        .await
        {
            Ok(t) => (true, None, t),
            Err(e) => (false, Some(format!("{:?}", e)), Vec::new()),
        };
        out.push(McpServerProbe {
            name: svc.name.clone(),
            url: svc.url.clone(),
            transport: svc.transport.clone(),
            ok,
            error,
            tools,
        });
    }
    out
}