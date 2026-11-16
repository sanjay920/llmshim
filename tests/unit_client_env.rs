//! The upstream client's relation to the environment (0021): a client built from
//! settings reads no `LLMSHIM_*` variable, and the shared client does.
//!
//! A test binary of its own with a single test: it sets process-wide variables,
//! and `set_var` while another thread reads the environment (reqwest reads the
//! proxy variables when it builds a client) is a data race, so no other test may
//! share the process.

use llmshim::client::{AttemptDeadlines, ClientSettings, ShimClient};
use std::time::Duration;

#[test]
fn a_client_from_settings_ignores_every_llmshim_variable_and_the_shared_one_does_not() {
    for (name, value) in [
        ("LLMSHIM_MAX_RETRIES", "9"),
        ("LLMSHIM_MAX_BACKOFF_SECS", "7"),
        ("LLMSHIM_UPSTREAM_CONNECT_TIMEOUT_MS", "1234"),
        ("LLMSHIM_UPSTREAM_HEADER_TIMEOUT_MS", "2345"),
        ("LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS", "3456"),
        ("LLMSHIM_UPSTREAM_UNARY_ATTEMPT_TIMEOUT_MS", "4567"),
        ("LLMSHIM_UPSTREAM_STREAM_IDLE_TIMEOUT_MS", "5678"),
    ] {
        std::env::set_var(name, value);
    }
    let from_settings = ShimClient::from_settings(ClientSettings::default()).unwrap();
    assert_eq!(
        from_settings.settings(),
        ClientSettings::default(),
        "explicit settings are not touched by the environment"
    );
    // The environment-driven client does read them, so the canaries are live.
    let from_env = ShimClient::new().settings();
    assert_eq!(from_env.max_retries, 9);
    assert_eq!(from_env.max_backoff, Duration::from_secs(7));
    assert_ne!(from_env.deadlines, AttemptDeadlines::default());
}
