use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub store: StoreConfig,
    pub gateway: GatewayConfig,
    pub claude: ClaudeConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StoreConfig {
    pub address: String,
}

/// The api0 gateway — where tool calls actually go. Every message runs as an
/// MCP client of it, with the linked person's own key.
#[derive(Debug, Deserialize, Clone)]
pub struct GatewayConfig {
    pub address: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeConfig {
    pub model: String,
    pub max_tokens: u32,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }
}
