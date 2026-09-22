//! Configuration file management for llmshim.
//!
//! Config file location: `~/.llmshim/config.toml`
//!
//! Precedence (highest to lowest):
//! 1. Environment variables (OPENAI_API_KEY, etc.)
//! 2. `~/.llmshim/config.toml`

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

/// The full config file structure.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub keys: Keys,

    #[serde(default)]
    pub proxy: ProxyConfig,

    /// Named routes, addressed as `route/<name>` in a request's `model`.
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
}

/// A caller-named route: one model plus request settings.
///
/// The name is arbitrary and llmshim never interprets it. A harness is free to
/// call a route `compaction`, `advisor` or `cheap`; llmshim only knows the name
/// maps to a model. Teaching the shim a fixed vocabulary of roles would pull
/// harness concepts below the abstraction they belong to — the harness decides
/// what `compaction` means, llmshim provides the mechanism.
///
/// ```toml
/// [routes.compaction]
/// model = "anthropic/claude-haiku-4-5-20251001"
/// reasoning_effort = "low"
/// max_tokens = 4096
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Route {
    /// The model this route resolves to, in any spelling the router accepts.
    pub model: String,
    /// Request settings applied when the request does not set them itself, so a
    /// per-request value always wins over the route's default.
    #[serde(flatten, default)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Keys {
    pub openai: Option<String>,
    pub anthropic: Option<String>,
    pub gemini: Option<String>,
    pub xai: Option<String>,
    #[serde(default)]
    pub openrouter: Option<String>,
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
        // A malformed file must be loud. Falling back silently would drop the
        // caller's API keys as well as the section they mistyped, and surface
        // only as "unknown provider" with nothing pointing at the real cause.
        Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
            let location = error
                .span()
                .map(|span| toml_error_location(&contents, span.start))
                .unwrap_or_else(|| "unknown location".to_string());
            eprintln!(
                "warning: {} has a TOML configuration error at {location} and was ignored; \
                 API keys and routes from it are not in effect",
                path.display()
            );
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn toml_error_location(contents: &str, error_offset: usize) -> String {
    let mut line_number = 1;
    let mut column_number = 1;
    for (character_offset, character) in contents.char_indices() {
        if character_offset >= error_offset {
            break;
        }
        if character == '\n' {
            line_number += 1;
            column_number = 1;
        } else {
            column_number += 1;
        }
    }
    format!("line {line_number}, column {column_number}")
}

/// Save config to ~/.llmshim/config.toml. Creates the directory if needed.
pub fn save(config: &Config) -> std::io::Result<()> {
    let configuration_directory = config_dir();
    let mut configuration_directory_builder = std::fs::DirBuilder::new();
    configuration_directory_builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        configuration_directory_builder.mode(0o700);
    }
    configuration_directory_builder.create(&configuration_directory)?;
    restrict_config_directory_permissions(&configuration_directory)?;
    let serialized_configuration = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    let mut temporary_config_file = tempfile::NamedTempFile::new_in(&configuration_directory)?;
    temporary_config_file.write_all(serialized_configuration.as_bytes())?;
    temporary_config_file.as_file().sync_all()?;
    temporary_config_file
        .persist(config_path())
        .map(|_| ())
        .map_err(|persist_error| persist_error.error)
}

#[cfg(unix)]
fn restrict_config_directory_permissions(directory_path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(directory_path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_config_directory_permissions(_: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Apply config keys as environment variables (only if not already set).
/// This implements the precedence: env vars > config file.
pub fn apply_to_env(config: &Config) {
    let mappings = [
        ("OPENAI_API_KEY", &config.keys.openai),
        ("ANTHROPIC_API_KEY", &config.keys.anthropic),
        ("GEMINI_API_KEY", &config.keys.gemini),
        ("XAI_API_KEY", &config.keys.xai),
        ("OPENROUTER_API_KEY", &config.keys.openrouter),
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
