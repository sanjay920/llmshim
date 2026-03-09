//! Shared environment/config loading for CLI and proxy binaries.
//!
//! Precedence (highest to lowest):
//! 1. Environment variables already set
//! 2. `.env` file in current directory
//! 3. `~/.llmshim/config.toml`

/// Load all config sources in precedence order.
/// After calling this, `std::env::var("OPENAI_API_KEY")` etc. will work
/// regardless of which source provided the value.
pub fn load_all() {
    // 1. Load ~/.llmshim/config.toml first (lowest priority)
    let config = crate::config::load();
    crate::config::apply_to_env(&config);

    // 2. Load .env file (overrides config.toml)
    load_dotenv();

    // Note: actual env vars already set by the shell have highest priority
    // because apply_to_env and load_dotenv both skip vars that are already set.
}

/// Load .env file from current directory, skipping vars already set.
fn load_dotenv() {
    let contents = match std::fs::read_to_string(".env") {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            // Strip surrounding quotes
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            // Only set if not already set (env vars take precedence)
            if std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
}
