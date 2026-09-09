use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
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
    /// Name of an environment variable holding a bearer token to send as
    /// `Authorization: Bearer <token>` on every request to this server.
    #[serde(default)]
    pub bearer_env: Option<String>,
}

fn default_mcp_transport() -> String {
    "http-streamable".to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_model")]
    pub default_model: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerConfig {
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_idle_timeout() -> u64 {
    300
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
            dirs::runtime_dir()
                .unwrap_or_else(|| std::env::temp_dir().join(format!("jancode-{}", std::process::id())))
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
    let cfg: Config = toml::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}
