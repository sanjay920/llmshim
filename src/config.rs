//! Configuration file management for llmshim.
//!
//! Config file location: `~/.llmshim/config.toml`
//!
//! Precedence (highest to lowest):
//! 1. Environment variables (OPENAI_API_KEY, etc.)
//! 2. `~/.llmshim/config.toml`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The full config file structure.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub keys: Keys,

    #[serde(default)]
    pub proxy: ProxyConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Keys {
    pub openai: Option<String>,
    pub anthropic: Option<String>,
    pub gemini: Option<String>,
    pub xai: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    3000
}

/// Get the config directory path (~/.llmshim/).
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".llmshim")
}

/// Get the config file path (~/.llmshim/config.toml).
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Load config from ~/.llmshim/config.toml. Returns default if file doesn't exist.
pub fn load() -> Config {
    let path = config_path();
    if !path.exists() {
        return Config::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

/// Save config to ~/.llmshim/config.toml. Creates the directory if needed.
pub fn save(config: &Config) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = config_path();
    let contents = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    std::fs::write(&path, contents)
}

/// Apply config keys as environment variables (only if not already set).
/// This implements the precedence: env vars > config file.
pub fn apply_to_env(config: &Config) {
    let mappings = [
        ("OPENAI_API_KEY", &config.keys.openai),
        ("ANTHROPIC_API_KEY", &config.keys.anthropic),
        ("GEMINI_API_KEY", &config.keys.gemini),
        ("XAI_API_KEY", &config.keys.xai),
    ];
    for (env_key, value) in mappings {
        if std::env::var(env_key).is_err() {
            if let Some(val) = value {
                if !val.is_empty() {
                    std::env::set_var(env_key, val);
                }
            }
        }
    }
}
