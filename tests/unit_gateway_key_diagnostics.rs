#![cfg(feature = "gateway")]

#[test]
fn gateway_key_loader_child() {
    if std::env::var_os("LLMSHIM_TEST_GATEWAY_KEY_LOADER").is_none() {
        return;
    }
    assert!(llmshim::gateway::auth::KeyStore::from_env().is_enforced());
}

#[test]
fn malformed_gateway_key_files_do_not_echo_credentials() {
    let synthetic_credential = "synthetic-gateway-secret";
    let temporary_directory = tempfile::tempdir().unwrap();
    let keys_path = temporary_directory.path().join("keys.json");
    for keys_value in [
        serde_json::json!(synthetic_credential),
        serde_json::json!({(synthetic_credential): synthetic_credential}),
        serde_json::json!({(synthetic_credential): {"tenant": "test", "rpm": synthetic_credential}}),
    ] {
        std::fs::write(&keys_path, serde_json::to_vec(&keys_value).unwrap()).unwrap();
        let child_output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gateway_key_loader_child", "--nocapture"])
            .env_clear()
            .env("LLMSHIM_TEST_GATEWAY_KEY_LOADER", "1")
            .env("LLMSHIM_GATEWAY_KEYS_FILE", &keys_path)
            .output()
            .unwrap();
        assert_eq!(child_output.status.code(), Some(1));
        let stderr = String::from_utf8(child_output.stderr).unwrap();
        let stdout = String::from_utf8(child_output.stdout).unwrap();
        assert!(stderr.contains("invalid keys file"));
        assert!(stderr.contains("at line 1, column "));
        assert!(stderr.contains("credentials were not loaded"));
        assert!(!stderr.contains(synthetic_credential));
        assert!(!stdout.contains(synthetic_credential));
        assert!(!stderr.contains("Auth: OPEN"));
    }

    std::fs::write(
        &keys_path,
        serde_json::to_vec(&serde_json::json!({(synthetic_credential): {"tenant": "test"}}))
            .unwrap(),
    )
    .unwrap();
    let valid_child_output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "gateway_key_loader_child", "--nocapture"])
        .env_clear()
        .env("LLMSHIM_TEST_GATEWAY_KEY_LOADER", "1")
        .env("LLMSHIM_GATEWAY_KEYS_FILE", &keys_path)
        .output()
        .unwrap();
    assert!(valid_child_output.status.success());
    let valid_stderr = String::from_utf8(valid_child_output.stderr).unwrap();
    assert!(valid_stderr.contains("Auth: ENFORCED (1 API key(s)"));
    assert!(!valid_stderr.contains(synthetic_credential));
}
