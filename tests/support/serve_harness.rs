//! Shared harness for the `llmshim serve` end-to-end tests: start a real server
//! process from a configuration, with a mock upstream, and talk to it over HTTP.
#![allow(dead_code)]

use serde_json::json;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub const CALLER_KEY: &str = "caller-key-0123456789abcdef";
pub const UPSTREAM_KEY: &str = "upstream-key-0123456789abcdef";
pub const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef";

pub struct Server {
    pub child: Child,
    /// Every line the server has written to stderr so far, kept after the start
    /// so a test can look for a panic or a leaked secret once it is done.
    pub stderr: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// The thread filling `stderr`; joined by [`Server::finished_stderr`] once the
    /// process has exited, so the last lines (a panic at shutdown) are not missed.
    reader: Option<std::thread::JoinHandle<()>>,
    pub base: String,
    /// The admin listener's address, when the configuration has `[admin]`.
    pub admin: Option<String>,
    /// The whole `Admin:` banner line, which says whether a token is required.
    pub admin_banner: String,
    pub dir: tempdir::Dir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch directory that removes itself.
pub mod tempdir {
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

pub fn start(config: &str, label: &str, extra_env: &[(&str, &str)]) -> Server {
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
        .env("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)
        .envs(extra_env.iter().copied())
        // Canaries: each of these would break or refuse requests if `serve`
        // read it. The file is the only source of settings.
        .env(
            "LLMSHIM_GATEWAY_KEYS_FILE",
            dir.0.join("does-not-exist.json"),
        )
        .env("LLMSHIM_GATEWAY_QUEUE_DEPTH", "0")
        .env("LLMSHIM_RATE_LIMIT_RPM", "0")
        .env("LLMSHIM_REDIS_URL", "redis://127.0.0.1:1")
        // The fleet tuning variables: each would make a fleet replica unusable
        // if `serve` read it (a 1 ms Redis deadline, a 1 ms job deadline and
        // lease), so the fleet tests also prove they are not read.
        .env("LLMSHIM_GATEWAY_REDIS_OPERATION_TIMEOUT_MS", "1")
        .env("LLMSHIM_GATEWAY_WORKER_JOB_TIMEOUT_MS", "1")
        .env("LLMSHIM_GATEWAY_ORIGIN_LEASE_MS", "1")
        .env("LLMSHIM_GATEWAY_RETAINED_JOBS", "1")
        .env("LLMSHIM_REPLAY_RECEIPTS_LOCK_TIMEOUT_MS", "1")
        .env("LLMSHIM_GATEWAY_MAX_RETAINED_ACCOUNTING_ATTEMPTS", "1")
        // The upstream client and the listener come from the file (0021). Each of
        // these would break every request or connection if `serve` read it: a
        // 1 ms attempt, connect and policy deadline, one inbound connection, a
        // 1 ms header deadline.
        .env("LLMSHIM_UPSTREAM_UNARY_ATTEMPT_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_STREAM_ATTEMPT_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_CONNECT_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_HEADER_TIMEOUT_MS", "1")
        .env("LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_ERROR_BODY_IDLE_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_ERROR_BODY_TOTAL_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_UNARY_IDLE_TIMEOUT_MS", "1")
        .env("LLMSHIM_UPSTREAM_STREAM_IDLE_TIMEOUT_MS", "1")
        // Many retries and a long backoff: the retry test below would see ten
        // calls and wait seconds if `serve` read these.
        .env("LLMSHIM_MAX_RETRIES", "9")
        .env("LLMSHIM_MAX_BACKOFF_SECS", "7")
        .env("LLMSHIM_HTTP_MAX_CONNECTIONS", "1")
        .env("LLMSHIM_HTTP_HEADER_TIMEOUT_MS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Owned by the guard from here on, so a failed start still reaps the process.
    let wants_admin = config.contains("[admin]");
    let mut server = Server {
        child,
        stderr: Default::default(),
        reader: None,
        base: String::new(),
        admin: None,
        admin_banner: String::new(),
        dir,
    };
    let stderr = server.child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let kept = server.stderr.clone();
    server.reader = Some(std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            kept.lock().unwrap().push(line.clone());
            let _ = sender.send(line);
        }
    }));
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(base) = line.split("listening on ").nth(1) {
                    server.base = base.trim().to_string();
                }
                if let Some(admin) = line.trim().strip_prefix("Admin: ") {
                    server.admin = admin.split_whitespace().next().map(str::to_string);
                    server.admin_banner = line.trim().to_string();
                }
                // The banner prints the main address, then the admin line.
                if !server.base.is_empty() && (!wants_admin || server.admin.is_some()) {
                    return server;
                }
                seen.push(line);
            }
            Err(_) => panic!("server did not start; stderr: {seen:#?}"),
        }
    }
}

pub fn config(upstream: &str, extra: &str) -> String {
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

pub fn completion(text: &str) -> String {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "test-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    })
    .to_string()
}

pub async fn chat(server: &Server, key: Option<&str>, model: &str) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.base))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

/// Ask the server to shut down, as an orchestrator's `SIGTERM` does.
#[cfg(unix)]
pub fn terminate(server: &Server) {
    let status = Command::new("kill")
        .args(["-TERM", &server.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

impl Server {
    /// Every line the server wrote to stderr, once its process has exited: the
    /// reader is joined first, so the last lines are included.
    pub fn finished_stderr(&mut self) -> Vec<String> {
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        self.stderr.lock().unwrap().clone()
    }
}

/// Wait up to `limit` for the server process to exit and return how it exited
/// (`None` if it is still running).
pub async fn exit_status_within(
    server: &mut Server,
    limit: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A chat request that sends its own `x-request-id` (or none).
pub async fn chat_with_request_id(
    server: &Server,
    key: Option<&str>,
    model: &str,
    request_id: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.base))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    if let Some(id) = request_id {
        request = request.header("x-request-id", id);
    }
    request.send().await.unwrap()
}

/// Ask the server to reload its configuration, as an operator's `kill -HUP` does.
#[cfg(unix)]
pub fn hangup(server: &Server) {
    let status = Command::new("kill")
        .args(["-HUP", &server.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Rewrite the configuration file the server started from.
pub fn rewrite(server: &Server, config: &str) {
    let path = server.dir.0.join("gateway.toml");
    std::fs::write(
        path,
        config.replace("{DIR}", server.dir.0.to_str().unwrap()),
    )
    .unwrap();
}

/// What the admin listener says about reloads.
pub async fn reload_status(server: &Server) -> serde_json::Value {
    let admin = server.admin.clone().expect("a configuration with [admin]");
    reqwest::Client::new()
        .get(format!("{admin}/v1/serve/config"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll the reload status until `done` accepts it, or fail after 20 seconds.
pub async fn wait_for_reload(
    server: &Server,
    what: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = reload_status(server).await;
        if done(&status) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; last status: {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run `serve` to its end. A regression that starts serving would never end,
/// so the wait is bounded and the process killed instead of hanging CI.
pub fn run_to_exit(config_path: &std::path::Path, env: &[(&str, &str)]) -> (Option<i32>, String) {
    run_with_args(config_path, &[], env)
}

/// [`run_to_exit`] with extra command-line arguments (`--check`, say).
pub fn run_with_args(
    config_path: &std::path::Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_llmshim"));
    command
        .args(["serve", "--config", config_path.to_str().unwrap()])
        .args(args)
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
