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
        ("SERVE_TEST_CALLER_KEY", "caller-secret-value-0123"),
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
        assert_eq!(
            output.status.code(),
            Some(3),
            "a secret that is not here is the environment's problem, not the file's"
        );
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
        assert!(stderr.contains("keys[0].secret: must be at least 22 characters"));
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
    fn arguments_are_validated_before_any_file_is_read() {
        let missing_config = run(&["serve", "--check"], &[]);
        assert_eq!(missing_config.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&missing_config.stderr).contains("--config"));
        let unknown = run(&["serve", "--config", "x.toml", "--port", "1"], &[]);
        assert_eq!(unknown.status.code(), Some(2));
        let help = run(&["serve", "--help"], &[]);
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("Usage: llmshim serve"));
        for documented in ["--structure-only", "--print-schema", "Exit status"] {
            assert!(help.contains(documented), "{documented} is in --help");
        }
    }

    /// The environment of a machine that holds no secret: nothing the file names.
    fn no_secrets(args: &[&str]) -> std::process::Output {
        run(args, &[])
    }

    #[test]
    fn structure_only_needs_no_secret_certificate_or_environment() {
        // A certificate, a key and a secret file that do not exist here, and an
        // environment that holds nothing: the machine of a pipeline that has been
        // given no secret.
        let config = format!(
            "{CONFIG}\n[server.tls]\ncert_file = \"/nonexistent/server.pem\"\n\
             key_file = \"/nonexistent/server.key\"\n\
             [[keys]]\nname = \"later\"\ntenant = \"other\"\n\
             secret = {{ file = \"/nonexistent/caller.key\" }}\n"
        );
        let path = write("structure.toml", &config);
        let output = no_secrets(&[
            "serve",
            "--config",
            path.to_str().unwrap(),
            "--check",
            "--structure-only",
        ]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(0), "{text}");
        assert!(text.contains("valid structure (version 1, 1 provider(s), 2 key(s), 0 route(s);"));
        assert!(
            text.contains("not checked"),
            "it says what it did not check"
        );
        // The same file, checked in full on the same machine: the secrets are not
        // available (3). A missing certificate is a failure of its own (1), which
        // is why a pipeline without them uses --structure-only.
        let full = no_secrets(&["serve", "--config", path.to_str().unwrap(), "--check"]);
        assert_eq!(full.status.code(), Some(3));
    }

    #[test]
    fn structure_only_still_refuses_a_file_that_is_wrong() {
        let path = write(
            "structure-bad.toml",
            &format!(
                "{CONFIG}
[server]
port = 70000
surprise = true
"
            ),
        );
        let output = no_secrets(&[
            "serve",
            "--config",
            path.to_str().unwrap(),
            "--check",
            "--structure-only",
        ]);
        assert_eq!(output.status.code(), Some(1));
        // A rule that needs no secret is applied too, not only parse errors: a
        // host that is not an address passes the parser and fails validation.
        let wrong_host = write(
            "structure-host.toml",
            &format!("{CONFIG}\n[server]\nhost = \"nope\"\n"),
        );
        let output = no_secrets(&[
            "serve",
            "--config",
            wrong_host.to_str().unwrap(),
            "--check",
            "--structure-only",
        ]);
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("server.host:"), "{stderr}");
    }

    #[test]
    fn a_defect_outranks_an_absence_in_the_exit_status() {
        // The caller token equals the upstream key (a defect in what was
        // configured) and a second key's secret is not set (an absence).
        let path = write(
            "both.toml",
            &format!(
                "{CONFIG}\n[[keys]]\nname = \"later\"\ntenant = \"other\"\n\
                 secret = {{ env = \"SERVE_TEST_NOT_PROVISIONED\" }}\n"
            ),
        );
        let output = run(
            &["serve", "--config", path.to_str().unwrap(), "--check"],
            &[
                ("SERVE_TEST_PROVIDER_KEY", "one-value-used-twice-0123456789"),
                ("SERVE_TEST_CALLER_KEY", "one-value-used-twice-0123456789"),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("SERVE_TEST_NOT_PROVISIONED"),
            "both are reported"
        );
    }

    #[test]
    fn starting_without_check_has_the_same_statuses() {
        // No server is started in any of these: the secrets are checked first.
        let path = write("start.toml", CONFIG);
        let missing = no_secrets(&["serve", "--config", path.to_str().unwrap()]);
        assert_eq!(missing.status.code(), Some(3), "an unset secret");
        let blank = run(
            &["serve", "--config", path.to_str().unwrap()],
            &[
                ("SERVE_TEST_PROVIDER_KEY", "   "),
                ("SERVE_TEST_CALLER_KEY", "a-caller-key-0123456789abc"),
            ],
        );
        assert_eq!(blank.status.code(), Some(3), "a blank secret");
        let unreadable = write(
            "unreadable.toml",
            &CONFIG.replace(
                "{ env = \"SERVE_TEST_CALLER_KEY\" }",
                "{ file = \"/nonexistent/caller.key\" }",
            ),
        );
        let output = run(
            &["serve", "--config", unreadable.to_str().unwrap()],
            &[("SERVE_TEST_PROVIDER_KEY", "provider-key-0123456789abc")],
        );
        assert_eq!(output.status.code(), Some(3), "an unreadable secret file");
        let weak = run(
            &["serve", "--config", path.to_str().unwrap()],
            &[
                ("SERVE_TEST_PROVIDER_KEY", "provider-key-0123456789abc"),
                ("SERVE_TEST_CALLER_KEY", "too-short"),
            ],
        );
        assert_eq!(weak.status.code(), Some(1), "a secret that breaks a rule");
    }

    #[test]
    fn a_secret_that_is_missing_does_not_hide_a_rule_broken_by_another() {
        // keys[0] is present but too short; keys[1] is not provisioned. Both are
        // reported and the status is the defect's, not the absence's.
        let path = write(
            "mixed.toml",
            &format!(
                "{CONFIG}\n[[keys]]\nname = \"later\"\ntenant = \"other\"\n\
                 secret = {{ env = \"SERVE_TEST_NOT_PROVISIONED\" }}\n"
            ),
        );
        let output = run(
            &["serve", "--config", path.to_str().unwrap(), "--check"],
            &[
                ("SERVE_TEST_PROVIDER_KEY", "provider-key-0123456789abc"),
                ("SERVE_TEST_CALLER_KEY", "too-short"),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("keys[0].secret: must be at least"),
            "{stderr}"
        );
        assert!(stderr.contains("SERVE_TEST_NOT_PROVISIONED"), "{stderr}");
        assert!(!stderr.contains("too-short"), "never the value");
    }

    #[test]
    fn structure_only_is_a_mode_of_check() {
        let path = write("mode.toml", CONFIG);
        let output = no_secrets(&[
            "serve",
            "--config",
            path.to_str().unwrap(),
            "--structure-only",
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("--structure-only needs --check"));
    }

    #[test]
    fn print_schema_matches_the_checked_in_contract() {
        let output = no_secrets(&["serve", "--print-schema"]);
        assert_eq!(output.status.code(), Some(0));
        let printed = String::from_utf8(output.stdout).unwrap();
        let checked_in = include_str!("../docs/design/enterprise-gateway/serve-config.schema.json");
        assert!(
            printed == checked_in,
            "the configuration schema changed. If that is intended, regenerate it:\n  \
             cargo run --features serve -- serve --print-schema > \\\n  \
             docs/design/enterprise-gateway/serve-config.schema.json\n\
             and say in the pull request which field changed and why a consumer is unaffected. \
             (A schemars update can change the output too: that is a reason to review it, \
             not to skip the comparison.)"
        );
        let schema: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(schema["additionalProperties"], false, "the root is closed");
    }

    #[test]
    fn print_schema_stands_alone() {
        for extra in [["--config", "x.toml"], ["--check", ""]] {
            let mut args = vec!["serve", "--print-schema"];
            args.extend(extra.iter().filter(|a| !a.is_empty()));
            let output = no_secrets(&args);
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("takes no other argument"));
        }
    }
}
