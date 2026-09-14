use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    /// Optional list of named providers for multi-provider setups. When
    /// non-empty, `default_provider` selects the active one; otherwise the
    /// single `[provider]` table is used (backward compatible).
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Name of the active provider from `providers` (ignored when `providers`
    /// is empty). Defaults to the first entry.
    #[serde(default)]
    pub default_provider: String,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
}

/// Optional database used by the `sql` tool. When a query runs without an
/// explicit `db`, this URL is used. Supported schemes: `postgres://`,
/// `mysql://`, or a `sqlite:`/plain file path.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub url: String,
}

/// MCP server config. Transport is currently `http-streamable` (the modern
/// streamable HTTP transport). `sse` is not yet supported.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    /// Inline bearer token sent as `Authorization: Bearer <token>` on every
    /// request to this server. Easiest when the daemon runs as a service (no
    /// env juggling); takes precedence over `bearer_env` when both are set.
    #[serde(default)]
    pub bearer: Option<String>,
    /// Name of an environment variable holding a bearer token to send as
    /// `Authorization: Bearer <token>` on every request to this server.
    /// Used when `bearer` is not set.
    #[serde(default)]
    pub bearer_env: Option<String>,
}

fn default_mcp_transport() -> String {
    "http-streamable".to_string()
}

/// Stall detection for provider streaming. The provider reads chunk-by-chunk;
/// if no chunk arrives for this duration we surface an `Elapsed` error and retry
/// once (so a stalled long-context generation doesn't block interactive turns).
/// Can be overridden for tests with `JANCODE_STREAM_STALL_SECS`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderConfig {
    /// Name of this provider (used in `[[providers]]` lists and `/provider`).
    /// Empty for the legacy single `[provider]` table.
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_model")]
    pub default_model: String,
    /// Optional hardcoded model catalog shown by the `/model` picker. When
    /// empty, jancode queries the provider's OpenAI-compatible `GET /models`
    /// endpoint instead.
    #[serde(default)]
    pub models: Vec<String>,
    /// How tool-call history is sent back to the provider. Some providers
    /// (e.g. Gemma via LM Studio) reject the OpenAI `tool_calls`/`tool` message
    /// roles in the request. Values:
    ///   - "openai":    send `tool_calls` + `tool` roles (default; works with
    ///                  OpenAI and most providers).
    ///   - "flattened": convert tool-call history into plain user/assistant
    ///                  messages (for providers that reject tool roles).
    ///   - "auto":      start with "openai"; if the provider rejects the tool
    ///                  roles or emits malformed tool calls, fall back to
    ///                  "flattened" automatically.
    #[serde(default = "default_tool_call_style")]
    pub tool_call_style: String,
    /// Whether this provider/model supports structured tool calling at all.
    /// When `false`, jancode never sends `tools` in the request and instead
    /// relies on the model answering directly (no tool execution). Some
    /// reasoning models / free routers don't implement function calling
    /// reliably; set this to `false` to avoid tool-call loops. Defaults to
    /// `true` (assume tool support).
    #[serde(default = "default_supports_tools")]
    pub supports_tools: bool,
    /// Optional cap on the number of tokens the model may generate in a single
    /// response. Sent as `max_tokens` in the request. `0`/unset means no cap.
    #[serde(default)]
    pub max_tokens: u64,
    /// Optional context window (in tokens) for the model. When set, jancode
    /// trims the oldest conversation messages to keep the request within this
    /// budget (reduces token usage and rate-limit pressure). `0`/unset means no
    /// trimming.
    #[serde(default)]
    pub context_window: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerConfig {
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Approval policy for risky tool calls (file writes/edits/patches, and
    /// reads outside the working directory). Values:
    ///   - "auto":   always allow (no prompts).
    ///   - "prompt": ask interactive sessions; headless `run`/swarm auto-allow.
    ///   - "deny":   always deny risky calls.
    #[serde(default = "default_approve_mode")]
    pub approve_mode: String,
    /// How aggressively the `bash` tool is gated when its command references
    /// files/directories outside the working directory. Values:
    ///   - "off":    never gate `bash` (old behavior).
    ///   - "basic":  gate on obvious escapes (absolute paths, `~`, `$HOME`,
    ///               `..`, leading `cd` out of the workspace). Default.
    ///   - "strict": also gate on any `cd`, `$PWD`/`$OLDPWD` tricks, and
    ///               commands that read env vars pointing outside.
    #[serde(default = "default_bash_gate")]
    pub bash_gate: String,
    /// Whether to print the model's reasoning/thinking tokens as `[thinking]`
    /// lines in interactive mode. Defaults to `false` (hidden) for a clean,
    /// light CLI. Set to `true` to debug what the model is working through.
    #[serde(default = "default_show_thinking")]
    pub show_thinking: bool,
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_tool_call_style() -> String {
    "openai".to_string()
}

fn default_supports_tools() -> bool {
    true
}

fn default_idle_timeout() -> u64 {
    300
}

fn default_approve_mode() -> String {
    "prompt".to_string()
}

fn default_bash_gate() -> String {
    "basic".to_string()
}

fn default_show_thinking() -> bool {
    false
}

pub fn jancode_dir() -> PathBuf {
    std::env::var("JANCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap().join(".jancode"))
}

pub fn config_path() -> PathBuf {
    jancode_dir().join("config.toml")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var("JANCODE_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Prefer the systemd-managed runtime dir (/run/jancode) when it
            // exists, so a client run from the shell connects to the same
            // daemon the systemd service started. Otherwise fall back to the
            // XDG runtime dir (per-user) or a temp dir.
            let systemd_dir = PathBuf::from("/run/jancode");
            if systemd_dir.exists() {
                systemd_dir
            } else {
                dirs::runtime_dir()
                    .unwrap_or_else(|| std::env::temp_dir().join(format!("jancode-{}", std::process::id())))
            }
        })
}

pub fn sessions_dir() -> PathBuf {
    jancode_dir().join("sessions")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let data = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
    // If `default_provider` wasn't deserialized (empty), fall back to the first
    // provider's name so `resolve_provider` picks a sensible default.
    if cfg.default_provider.is_empty() && !cfg.providers.is_empty() {
        cfg.default_provider = cfg.providers[0].name.clone();
    }
    Ok(cfg)
}

/// Resolve the active provider config. If `providers` is non-empty, returns the
/// entry named by `default_provider` (or the first entry if not found). Falls
/// back to the legacy single `[provider]` table otherwise.
pub fn resolve_provider(cfg: &Config, name: Option<&str>) -> ProviderConfig {
    if !cfg.providers.is_empty() {
        let want = name.unwrap_or(&cfg.default_provider);
        if !want.is_empty() {
            if let Some(p) = cfg.providers.iter().find(|p| p.name == want) {
                return p.clone();
            }
        }
        // Fall back to the first provider.
        return cfg.providers[0].clone();
    }
    cfg.provider.clone()
}

/// List the names of all configured providers (for `/provider`).
pub fn provider_names(cfg: &Config) -> Vec<String> {
    if !cfg.providers.is_empty() {
        cfg.providers.iter().map(|p| p.name.clone()).collect()
    } else {
        vec!["default".to_string()]
    }
}
