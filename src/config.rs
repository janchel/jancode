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

#[derive(Debug, Clone, Deserialize)]
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
    /// Draw a border/box around the model's responses so they stand out from
    /// tool output and user input (helps a lot in long sessions). Values:
    ///   - "gutter": prefix each response line with a dim `│ ` bar. Default.
    ///   - "box":    like "gutter", plus top/bottom `─` rules around the reply.
    ///   - "none":   plain output (old behavior).
    /// Pure client-side rendering; no extra tokens or provider calls.
    #[serde(default = "default_response_border")]
    pub response_border: String,
    /// Dim the `[tool]` / `[approval]` / `[thinking]` status lines so they
    /// recede and the model's reply stands out. Terminal-only (no effect when
    /// piped). Default true.
    #[serde(default = "default_dim_tool_lines")]
    pub dim_tool_lines: bool,
    /// Show the lines the model changed, right below each successful
    /// `edit` / `apply_patch` tool call (a compact `-`/`+` diff, colourised on
    /// a terminal). Only the modified lines are shown — context lines are
    /// dropped. Rendered client-side from the tool-call arguments; costs no
    /// extra tokens or provider calls. Default true; set `false` to keep the
    /// session minimal.
    #[serde(default = "default_show_diffs")]
    pub show_diffs: bool,
    /// Maximum number of model turns (tool-calling iterations) in a single
    /// request before jancode gives up. Larger projects need more exploration
    /// turns. Default 50. The per-call repeat/error/denial guards still apply,
    /// so raising this doesn't enable runaway loops — just longer useful work.
    #[serde(default = "default_max_tool_loops")]
    pub max_tool_loops: u32,
    /// Maximum total tool calls in a single request before jancode stops (a
    /// secondary cap for a model that wanders across many *different* calls
    /// without producing a final answer). Default 75.
    #[serde(default = "default_max_total_tool_calls")]
    pub max_total_tool_calls: u32,
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

fn default_response_border() -> String {
    "gutter".to_string()
}

fn default_dim_tool_lines() -> bool {
    true
}

fn default_show_diffs() -> bool {
    true
}

fn default_max_tool_loops() -> u32 {
    50
}

fn default_max_total_tool_calls() -> u32 {
    75
}

// Hand-written so a config file with no `[server]` section gets the SAME
// defaults as an empty `[server]` (serde uses this impl for the missing field,
// while the `default = "..."` attrs cover missing individual keys). A derived
// `Default` would yield zeros (e.g. idle_timeout 0).
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout(),
            approve_mode: default_approve_mode(),
            bash_gate: default_bash_gate(),
            show_thinking: default_show_thinking(),
            response_border: default_response_border(),
            dim_tool_lines: default_dim_tool_lines(),
            show_diffs: default_show_diffs(),
            max_tool_loops: default_max_tool_loops(),
            max_total_tool_calls: default_max_total_tool_calls(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_defaults() {
        // An empty/minimal config gets sensible defaults.
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(cfg.server.max_tool_loops, 50);
        assert_eq!(cfg.server.max_total_tool_calls, 75);
        assert_eq!(cfg.server.response_border, "gutter");
        assert!(cfg.server.dim_tool_lines);
        assert!(cfg.server.show_diffs);
        assert_eq!(cfg.server.idle_timeout_secs, 300);
        assert_eq!(cfg.server.approve_mode, "prompt");
        assert_eq!(cfg.server.bash_gate, "basic");
    }

    #[test]
    fn server_limits_override() {
        let cfg: Config = toml::from_str(
            "[server]\nmax_tool_loops = 120\nmax_total_tool_calls = 200\nresponse_border = \"none\"\nshow_diffs = false\n",
        )
        .expect("override config parses");
        assert_eq!(cfg.server.max_tool_loops, 120);
        assert_eq!(cfg.server.max_total_tool_calls, 200);
        assert_eq!(cfg.server.response_border, "none");
        assert!(!cfg.server.show_diffs);
    }
}
