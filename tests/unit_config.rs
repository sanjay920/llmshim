use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn run_config_command(home_directory: &std::path::Path, key_name: &str) {
    let command_output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["set", key_name, "test-placeholder"])
        .env("HOME", home_directory)
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .output()
        .expect("config command should start");
    assert!(
        command_output.status.success(),
        "config command failed: {}",
        String::from_utf8_lossy(&command_output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn config_save_uses_private_directory_and_file_modes() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    run_config_command(temporary_home_directory.path(), "openai");

    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    let config_file_path = config_directory_path.join("config.toml");
    assert_eq!(
        std::fs::metadata(&config_directory_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&config_file_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn config_save_replaces_existing_permissive_file_with_private_modes() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    let config_file_path = config_directory_path.join("config.toml");
    std::fs::create_dir_all(&config_directory_path).unwrap();
    std::fs::write(&config_file_path, "[keys]\nopenai = \"old\"\n").unwrap();
    std::fs::set_permissions(
        &config_directory_path,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::set_permissions(&config_file_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    run_config_command(temporary_home_directory.path(), "anthropic");

    assert_eq!(
        std::fs::metadata(&config_directory_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&config_file_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let config_contents = std::fs::read_to_string(config_file_path).unwrap();
    assert!(config_contents.contains("anthropic = \"test-placeholder\""));
    assert!(config_contents.contains("openai = \"old\""));
}

#[test]
fn malformed_config_diagnostic_excludes_source_marker_and_retains_location() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    std::fs::create_dir_all(&config_directory_path).unwrap();
    let source_marker = "SYNTHETIC_CONFIG_DIAGNOSTIC_MARKER";
    std::fs::write(
        config_directory_path.join("config.toml"),
        format!("[keys]\nopenai = \"{source_marker}\n"),
    )
    .unwrap();

    let command_output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["get", "openai"])
        .env("HOME", temporary_home_directory.path())
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .output()
        .expect("config command should start");
    let diagnostic = String::from_utf8_lossy(&command_output.stderr);
    assert!(!diagnostic.contains(source_marker), "{diagnostic}");
    assert!(!diagnostic.contains("openai ="), "{diagnostic}");
    assert!(diagnostic.contains(
        &config_directory_path
            .join("config.toml")
            .display()
            .to_string()
    ));
    assert!(
        diagnostic.contains("TOML configuration error"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("line 2, column "), "{diagnostic}");
}
