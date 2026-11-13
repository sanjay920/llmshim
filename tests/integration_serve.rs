//! `llmshim serve` end to end: a real server process, a mock upstream, and real
//! HTTP. Runs offline; the "upstream" is a local mock.
#![cfg(feature = "serve")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CALLER_KEY: &str = "caller-key-0123456789abcdef";
const UPSTREAM_KEY: &str = "upstream-key-0123456789abcdef";

struct Server {
    child: Child,
    base: String,
    _dir: tempdir::Dir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch directory that removes itself.
mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Dir {
        pub fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "llmshim-serve-e2e-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn start(config: &str, label: &str) -> Server {
    let dir = tempdir::Dir::new(label);
    let path = dir.0.join("gateway.toml");
    std::fs::write(&path, config.replace("{DIR}", dir.0.to_str().unwrap())).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap()])
        .env_clear()
        .env("HOME", &dir.0)
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .env("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY)
        // Canaries: each of these would break or refuse requests if `serve`
        // read it. The file is the only source of settings.
        .env(
            "LLMSHIM_GATEWAY_KEYS_FILE",
            dir.0.join("does-not-exist.json"),
        )
        .env("LLMSHIM_GATEWAY_QUEUE_DEPTH", "0")
        .env("LLMSHIM_RATE_LIMIT_RPM", "0")
        .env("LLMSHIM_REDIS_URL", "redis://127.0.0.1:1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Owned by the guard from here on, so a failed start still reaps the process.
    let mut server = Server {
        child,
        base: String::new(),
        _dir: dir,
    };
    let stderr = server.child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(base) = line.split("listening on ").nth(1) {
                    server.base = base.trim().to_string();
                    return server;
                }
                seen.push(line);
            }
            Err(_) => panic!("server did not start; stderr: {seen:#?}"),
        }
    }
}

fn config(upstream: &str, extra: &str) -> String {
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.inference]
        kind = "openai-compatible"
        base_url = "{upstream}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
        {extra}
        "#
    )
}

fn completion(text: &str) -> String {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "test-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    })
    .to_string()
}

async fn chat(server: &Server, key: Option<&str>, model: &str) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.base))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn a_caller_with_a_key_reaches_the_upstream_with_the_upstream_key_only() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .match_header("authorization", format!("Bearer {UPSTREAM_KEY}").as_str())
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let server = start(&config(&upstream.url(), ""), "ok");

    let response = chat(&server, Some(CALLER_KEY), "inference/test-model").await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "pong");
    mock.assert_async().await;
}

#[tokio::test]
async fn missing_and_wrong_keys_are_refused_before_the_upstream_is_called() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_body(completion("unreachable"))
        .expect(0)
        .create_async()
        .await;
    let server = start(&config(&upstream.url(), ""), "deny");

    assert_eq!(
        chat(&server, None, "inference/test-model").await.status(),
        401
    );
    assert_eq!(
        chat(
            &server,
            Some("some-other-key-0123456789"),
            "inference/test-model"
        )
        .await
        .status(),
        401
    );
    // The upstream key is not a caller key.
    assert_eq!(
        chat(&server, Some(UPSTREAM_KEY), "inference/test-model")
            .await
            .status(),
        401
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn a_named_route_applies_its_model_and_default_settings() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(
            json!({"model": "test-model", "max_tokens": 77}),
        ))
        .with_header("content-type", "application/json")
        .with_body(completion("routed"))
        .expect(1)
        .create_async()
        .await;
    let extra = r#"
        [routes.fast]
        model = "inference/test-model"
        settings = { max_tokens = 77 }
    "#;
    let server = start(&config(&upstream.url(), extra), "route");

    let response = chat(&server, Some(CALLER_KEY), "route/fast").await;
    assert_eq!(response.status(), 200);
    mock.assert_async().await;
    // An undefined route is a client error, never a silent fall back.
    let unknown = chat(&server, Some(CALLER_KEY), "route/missing").await;
    assert!(unknown.status().is_client_error(), "{}", unknown.status());
}

#[tokio::test]
async fn the_gateway_surface_is_served_and_unauthenticated_probes_stay_open() {
    let upstream = mockito::Server::new_async().await;
    let server = start(&config(&upstream.url(), ""), "surface");
    let client = reqwest::Client::new();
    for path in ["/health", "/ready"] {
        let response = client
            .get(format!("{}{path}", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }
    let metrics = client
        .get(format!("{}/metrics", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
}

/// Run `serve` to its end. A regression that starts serving would never end,
/// so the wait is bounded and the process killed instead of hanging CI.
fn run_to_exit(config_path: &std::path::Path, env: &[(&str, &str)]) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_llmshim"));
    command
        .args(["serve", "--config", config_path.to_str().unwrap()])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = reader.join().unwrap();
            panic!("serve did not exit; stderr: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status, reader.join().unwrap())
}

#[test]
fn a_configuration_problem_exits_one_without_binding_a_port() {
    let dir = tempdir::Dir::new("bad");
    let path = dir.0.join("bad.toml");
    std::fs::write(&path, "version = 1\n[server]\nhost = \"nope\"\n").unwrap();
    let (code, stderr) = run_to_exit(&path, &[]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("server.host"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

#[test]
fn a_failed_start_creates_nothing_on_disk() {
    // A caller token that is too short is found while the application is
    // assembled, after the receipts directory used to be created.
    let dir = tempdir::Dir::new("nothing-created");
    let path = dir.0.join("bad.toml");
    let receipts = dir.0.join("receipts");
    std::fs::write(
        &path,
        format!(
            r#"
            version = 1
            [state]
            receipts_dir = "{}"
            [providers.inference]
            kind = "openai-compatible"
            base_url = "http://127.0.0.1:9/v1"
            api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
            [[keys]]
            name = "ci"
            tenant = "platform"
            secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
            "#,
            receipts.display()
        ),
    )
    .unwrap();
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
            ("SERVE_E2E_CALLER_KEY", "short"),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("keys[0].secret"), "{stderr}");
    assert!(!receipts.exists(), "a failed start must not create files");
}

#[cfg(unix)]
#[tokio::test]
async fn the_receipts_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let upstream = mockito::Server::new_async().await;
    let server = start(&config(&upstream.url(), ""), "private-dir");
    let mode = std::fs::metadata(server._dir.0.join("receipts"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "{mode:o}");
}

#[test]
fn a_caller_token_equal_to_an_upstream_key_is_refused_at_startup() {
    // `check_references` runs in `serve --check`; assembly repeats it so that a
    // caller of `build_app` cannot skip the rule.
    let dir = tempdir::Dir::new("same-value");
    let path = dir.0.join("bad.toml");
    std::fs::write(
        &path,
        r#"
        version = 1
        [providers.inference]
        kind = "openai-compatible"
        base_url = "http://127.0.0.1:9/v1"
        api_key = { env = "SERVE_E2E_UPSTREAM_KEY" }
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = { env = "SERVE_E2E_CALLER_KEY" }
        "#,
    )
    .unwrap();
    // Two different variables that hold the same value: only the values show it.
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
            ("SERVE_E2E_CALLER_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("same value as a provider"), "{stderr}");
    assert!(!stderr.contains(UPSTREAM_KEY), "{stderr}");
}
