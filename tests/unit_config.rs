#[cfg(unix)]
use std::process::Command;

#[cfg(unix)]
fn bounded_child_output(mut child: std::process::Child) -> std::process::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("secret-file reader blocked on a synthetic FIFO");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn create_fifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

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

#[cfg(unix)]
#[test]
fn config_load_repairs_legacy_default_file_and_directory_modes() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    let config_file_path = config_directory_path.join("config.toml");
    std::fs::create_dir_all(&config_directory_path).unwrap();
    std::fs::write(&config_file_path, "[keys]\nopenai = \"test-placeholder\"\n").unwrap();
    std::fs::set_permissions(
        &config_directory_path,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::set_permissions(&config_file_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let command_output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["get", "openai"])
        .env("HOME", temporary_home_directory.path())
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .output()
        .unwrap();

    assert!(command_output.status.success());
    assert!(!String::from_utf8_lossy(&command_output.stdout).contains("(not set)"));
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
fn config_load_rejects_a_default_path_symlink_without_reading_its_target() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    let target_path = temporary_home_directory.path().join("config-target.toml");
    std::fs::create_dir_all(&config_directory_path).unwrap();
    std::fs::write(&target_path, "[keys]\nopenai = \"target-value\"\n").unwrap();
    std::os::unix::fs::symlink(&target_path, config_directory_path.join("config.toml")).unwrap();

    let command_output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["get", "openai"])
        .env("HOME", temporary_home_directory.path())
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .output()
        .unwrap();

    assert!(command_output.status.success());
    assert!(String::from_utf8_lossy(&command_output.stdout).contains("(not set)"));
    assert!(String::from_utf8_lossy(&command_output.stderr)
        .contains("default configuration file could not be safely loaded"));
    assert_eq!(
        std::fs::read_to_string(target_path).unwrap(),
        "[keys]\nopenai = \"target-value\"\n"
    );
}

#[cfg(unix)]
#[test]
fn config_load_rejects_a_default_fifo_without_blocking() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    std::fs::create_dir_all(&config_directory_path).unwrap();
    create_fifo(&config_directory_path.join("config.toml"));

    let child = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["get", "openai"])
        .env("HOME", temporary_home_directory.path())
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let command_output = bounded_child_output(child);

    assert!(command_output.status.success());
    assert!(String::from_utf8_lossy(&command_output.stdout).contains("(not set)"));
    assert!(String::from_utf8_lossy(&command_output.stderr)
        .contains("default configuration file could not be safely loaded"));
}

#[cfg(unix)]
#[test]
fn config_load_rejects_a_default_hard_link_without_changing_its_target() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    let target_path = temporary_home_directory.path().join("config-target.toml");
    let target_contents = "[keys]\nopenai = \"target-value\"\n";
    std::fs::create_dir_all(&config_directory_path).unwrap();
    std::fs::write(&target_path, target_contents).unwrap();
    std::fs::set_permissions(&target_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::hard_link(&target_path, config_directory_path.join("config.toml")).unwrap();

    let command_output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["get", "openai"])
        .env("HOME", temporary_home_directory.path())
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .output()
        .unwrap();

    assert!(command_output.status.success());
    assert!(String::from_utf8_lossy(&command_output.stdout).contains("(not set)"));
    assert!(String::from_utf8_lossy(&command_output.stderr)
        .contains("default configuration file could not be safely loaded"));
    assert_eq!(
        std::fs::read_to_string(&target_path).unwrap(),
        target_contents
    );
    assert_eq!(
        std::fs::metadata(target_path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[cfg(unix)]
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
