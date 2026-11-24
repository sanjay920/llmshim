#![cfg(unix)]

use llmshim::providers::chatgpt::ChatGptAuth;
use std::process::Command;

const CHILD_PROBE_ENVIRONMENT_VARIABLE: &str = "LLMSHIM_TEST_CHATGPT_AUTH_OVERRIDE";

#[test]
fn chatgpt_auth_override_child() {
    if std::env::var_os(CHILD_PROBE_ENVIRONMENT_VARIABLE).is_none() {
        return;
    }
    let chatgpt_auth = ChatGptAuth::from_env();
    println!("{}", chatgpt_auth.auth_path().display());
}

fn probe_auth_path(
    home_directory: &std::path::Path,
    token_directory: Option<&std::path::Path>,
    auth_file: &std::path::Path,
) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "chatgpt_auth_override_child", "--nocapture"])
        .env_clear()
        .env(CHILD_PROBE_ENVIRONMENT_VARIABLE, "1")
        .env("HOME", home_directory)
        .env("CHATGPT_AUTH_FILE", auth_file);
    if let Some(token_directory) = token_directory {
        command.env("CHATGPT_TOKEN_DIR", token_directory);
    }
    command.output().unwrap()
}

#[test]
fn auth_file_override_preserves_absolute_and_relative_path_semantics() {
    let temporary_root = tempfile::tempdir().unwrap();
    let default_home = temporary_root.path().join("home");
    let explicit_token_directory = temporary_root.path().join("tokens");
    let absolute_auth_file = temporary_root.path().join("absolute-auth.json");

    let relative_without_token_directory =
        probe_auth_path(&default_home, None, std::path::Path::new("relative.json"));
    assert!(relative_without_token_directory.status.success());
    assert_eq!(
        String::from_utf8(relative_without_token_directory.stdout)
            .unwrap()
            .lines()
            .find(|line| line.ends_with("relative.json"))
            .unwrap(),
        default_home
            .join(".llmshim/chatgpt/relative.json")
            .display()
            .to_string()
    );

    let relative_with_token_directory = probe_auth_path(
        &default_home,
        Some(&explicit_token_directory),
        std::path::Path::new("relative.json"),
    );
    assert!(relative_with_token_directory.status.success());
    assert_eq!(
        String::from_utf8(relative_with_token_directory.stdout)
            .unwrap()
            .lines()
            .find(|line| line.ends_with("relative.json"))
            .unwrap(),
        explicit_token_directory
            .join("relative.json")
            .display()
            .to_string()
    );

    let absolute_override = probe_auth_path(&default_home, None, &absolute_auth_file);
    assert!(absolute_override.status.success());
    assert_eq!(
        String::from_utf8(absolute_override.stdout)
            .unwrap()
            .lines()
            .find(|line| line.ends_with("absolute-auth.json"))
            .unwrap(),
        absolute_auth_file.display().to_string()
    );
}
