#![cfg(all(feature = "proxy", feature = "redis-coordination"))]

#[test]
fn redis_startup_child() {
    if std::env::var_os("LLMSHIM_TEST_REDIS_STARTUP_CHILD").is_none() {
        return;
    }
    let _application_state =
        llmshim::proxy::AppState::from_env(llmshim::router::Router::new(), None);
}

#[test]
fn redis_startup_status_does_not_disclose_coordination_credentials() {
    for redis_scheme in ["redis", "rediss"] {
        let synthetic_redis_url = format!(
            "{redis_scheme}://diagnostic-user:synthetic-password%3Asecret@coordination.invalid:6379/0"
        );
        let child_output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "redis_startup_child", "--nocapture"])
            .env_clear()
            .env("LLMSHIM_TEST_REDIS_STARTUP_CHILD", "1")
            .env("LLMSHIM_REDIS_URL", synthetic_redis_url)
            .output()
            .unwrap();
        assert!(child_output.status.success());
        let stderr = String::from_utf8(child_output.stderr).unwrap();
        assert!(stderr.contains("rate limiting: redis coordination enabled"));
        assert!(stderr.contains("provider health: redis coordination enabled"));
        for private_value in [
            "diagnostic-user",
            "synthetic-password",
            "secret",
            "coordination.invalid",
        ] {
            assert!(
                !stderr.contains(private_value),
                "startup disclosed {private_value}"
            );
        }
    }
}
