//! Shared environment/config loading.
//!
//! Precedence (highest to lowest):
//! 1. Environment variables already set
//! 2. `~/.llmshim/config.toml`

/// Load config and apply to environment.
/// After calling this, `std::env::var("OPENAI_API_KEY")` etc. will work
/// regardless of which source provided the value.
pub fn load_all() {
    let config = crate::config::load();
    crate::config::apply_to_env(&config);
}
