use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn run(args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(args)
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "CLI did not exit: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn subcommand_help_exits_without_starting_a_server_or_chat() {
    for command in ["proxy", "gateway", "chat", "models", "configure", "docker"] {
        let output = run(&[command, "--help"]);
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("Usage:"));
        assert!(!stderr.contains("starting on"));
        assert!(!stderr.contains("No providers configured"));
    }
}
#[test]
fn unknown_flags_missing_values_and_extra_arguments_fail_without_side_effects() {
    for args in [
        vec!["proxy", "--unknown"],
        vec!["gateway", "--port"],
        vec!["proxy", "--port", "bad"],
        vec!["proxy", "--port", "65536"],
        vec!["proxy", "--host", "not-an-address"],
        vec!["chat", "--unknown"],
        vec!["chat", "--log"],
        vec!["models", "--bogus"],
        vec!["get", "key", "extra"],
        vec!["configure", "--unknown"],
        vec!["docker", "logs", "--unknown"],
        vec!["unknown-command"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.is_empty());
        assert!(!stderr.contains("starting on"));
        assert!(!stderr.contains("panicked"));
    }
}
