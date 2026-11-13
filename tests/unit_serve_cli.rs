//! `llmshim serve` exists only in builds with the `serve` feature, and never
//! starts a server while it is in development.
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_llmshim"));
    command
        .args(args)
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .env_remove("SERVE_TEST_PROVIDER_KEY")
        .env_remove("SERVE_TEST_CALLER_KEY")
        .stdin(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

#[cfg(not(feature = "serve"))]
#[test]
fn a_build_without_the_feature_does_not_know_the_command() {
    let output = run(&["serve", "--config", "gateway.toml", "--check"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "unknown command: serve"
    );
    let help = run(&["help"], &[]);
    assert!(!String::from_utf8_lossy(&help.stderr).contains("serve --config"));
}

#[cfg(feature = "serve")]
mod with_feature {
    use super::run;

    const CONFIG: &str = r#"
        version = 1
        [providers.openai]
        kind = "openai"
        api_key = { env = "SERVE_TEST_PROVIDER_KEY" }
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = { env = "SERVE_TEST_CALLER_KEY" }
    "#;

    fn write(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("llmshim-serve-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    const SECRETS: [(&str, &str); 2] = [
        ("SERVE_TEST_PROVIDER_KEY", "provider-secret-value"),
        ("SERVE_TEST_CALLER_KEY", "caller-secret-value"),
    ];

    #[test]
    fn check_accepts_a_valid_file_and_prints_no_secret() {
        let path = write("valid.toml", CONFIG);
        let output = run(
            &["serve", "--config", path.to_str().unwrap(), "--check"],
            &SECRETS,
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(0), "{text}");
        assert!(text.contains("valid (version 1, 1 provider(s), 1 key(s), 0 route(s))"));
        assert!(!text.contains("secret-value"));
    }

    #[test]
    fn check_reports_missing_secrets_by_setting() {
        let path = write("missing.toml", CONFIG);
        let output = run(
            &["serve", "--config", path.to_str().unwrap(), "--check"],
            &[],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr.contains("2 problem(s)"), "{stderr}");
        assert!(stderr.contains(
            "providers.openai.api_key: environment variable SERVE_TEST_PROVIDER_KEY is not set"
        ));
        assert!(stderr.contains("keys[0].secret"));
    }

    #[test]
    fn check_rejects_a_weak_caller_token_without_printing_it() {
        let path = write("weak.toml", CONFIG);
        let output = run(
            &["serve", "--config", path.to_str().unwrap(), "--check"],
            &[
                ("SERVE_TEST_PROVIDER_KEY", "provider-secret-value"),
                ("SERVE_TEST_CALLER_KEY", "short-weak"),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr.contains("keys[0].secret: must be at least 16 characters"));
        assert!(!stderr.contains("short-weak"));
    }

    #[test]
    fn an_invalid_file_fails_with_every_problem_listed() {
        let path = write("invalid.toml", "version = 1\n[server]\nhost = \"nope\"\n");
        let output = run(
            &["serve", "--config", path.to_str().unwrap(), "--check"],
            &[],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1));
        for path in ["server.host", "keys", "providers"] {
            assert!(stderr.contains(&format!("  {path}:")), "{stderr}");
        }
    }

    #[test]
    fn without_check_it_refuses_to_pretend_it_is_serving() {
        let path = write("noserve.toml", CONFIG);
        let output = run(&["serve", "--config", path.to_str().unwrap()], &SECRETS);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr.contains("not available in this build yet"));
        assert!(!stderr.contains("starting on"));
    }

    #[test]
    fn arguments_are_validated_before_any_file_is_read() {
        let missing_config = run(&["serve", "--check"], &[]);
        assert_eq!(missing_config.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&missing_config.stderr).contains("--config"));
        let unknown = run(&["serve", "--config", "x.toml", "--port", "1"], &[]);
        assert_eq!(unknown.status.code(), Some(2));
        let help = run(&["serve", "--help"], &[]);
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: llmshim serve"));
    }
}
