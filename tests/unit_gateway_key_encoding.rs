#![cfg(all(feature = "gateway", unix))]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

#[test]
fn gateway_key_encoding_child() {
    if std::env::var_os("LLMSHIM_TEST_GATEWAY_KEY_ENCODING").is_none() {
        return;
    }
    let key_store = llmshim::gateway::auth::KeyStore::from_env();
    std::process::exit(if key_store.is_enforced() { 0 } else { 7 });
}

fn probe_key_file(key_file: Option<OsString>) -> std::process::Output {
    let mut child_command = std::process::Command::new(std::env::current_exe().unwrap());
    child_command
        .args(["--exact", "gateway_key_encoding_child", "--nocapture"])
        .env_clear()
        .env("LLMSHIM_TEST_GATEWAY_KEY_ENCODING", "1");
    if let Some(key_file) = key_file {
        child_command.env("LLMSHIM_GATEWAY_KEYS_FILE", key_file);
    }
    child_command.output().unwrap()
}

#[test]
fn invalid_key_file_encoding_must_not_disable_authentication() {
    let mut invalid_path = b"synthetic-private-path-".to_vec();
    invalid_path.push(0xff);
    let child_output = probe_key_file(Some(OsString::from_vec(invalid_path)));
    assert_eq!(child_output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&child_output.stderr);
    assert!(!stderr.contains("Auth: OPEN"));
    assert!(!stderr.contains("synthetic-private-path"));
    assert!(stderr.contains("LLMSHIM_GATEWAY_KEYS_FILE"));
}

#[test]
fn key_file_configuration_preserves_explicit_auth_modes() {
    for absent_configuration in [None, Some(OsString::from("")), Some(OsString::from(" "))] {
        let child_output = probe_key_file(absent_configuration);
        assert_eq!(child_output.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&child_output.stderr).contains("Auth: OPEN"));
    }
    let temporary_directory = tempfile::tempdir().unwrap();
    let keys_path = temporary_directory.path().join("keys.json");
    std::fs::write(&keys_path, br#"{"synthetic-key":{"tenant":"test"}}"#).unwrap();
    let child_output = probe_key_file(Some(keys_path.into_os_string()));
    assert_eq!(child_output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&child_output.stderr).contains("Auth: ENFORCED"));
}
