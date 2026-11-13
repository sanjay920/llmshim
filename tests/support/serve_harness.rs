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
    pub base: String,
    /// The admin listener's address, when the configuration has `[admin]`.
    pub admin: Option<String>,
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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Owned by the guard from here on, so a failed start still reaps the process.
    let wants_admin = config.contains("[admin]");
    let mut server = Server {
        child,
        base: String::new(),
        admin: None,
        dir,
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
                }
                if let Some(admin) = line.trim().strip_prefix("Admin: ") {
                    server.admin = admin.split_whitespace().next().map(str::to_string);
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
