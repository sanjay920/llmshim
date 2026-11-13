//! `llmshim serve` reloading its caller keys: a real server process, the real
//! file rewritten under it, and `SIGHUP` (or the timer) asking for the change.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    chat, completion, config, hangup, reload_status, rewrite, start, tempdir, wait_for_reload,
    Server, ADMIN_TOKEN, CALLER_KEY, UPSTREAM_KEY,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SECOND: &str = "second-caller-key-0123456789abcdef";
const THIRD: &str = "third-caller-key-0123456789abcdef";
const MODEL: &str = "inference/test-model";

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn key(name: &str, tenant: &str, token: &str, extra: &str) -> String {
    format!(
        "[[keys]]\nname = \"{name}\"\ntenant = \"{tenant}\"\nsha256 = \"{}\"\n{extra}\n",
        digest(token)
    )
}

/// The harness configuration (one key, `ci`) with an admin listener, `top`
/// settings and `keys` appended.
fn render(upstream: &str, top: &str, keys: &str) -> String {
    config(
        upstream,
        &format!("{top}\n[admin]\nport = 0\ntoken = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n{keys}"),
    )
}

/// One HTTP/1.1 connection that stays open across requests.
struct Raw {
    stream: TcpStream,
}

impl Raw {
    async fn connect(server: &Server) -> Self {
        let address = server.base.trim_start_matches("http://");
        Self {
            stream: TcpStream::connect(address).await.unwrap(),
        }
    }

    /// Send one chat request with `token` and return its status, reading the whole
    /// response so the next request starts cleanly on the same connection.
    async fn status(&mut self, token: &str) -> u16 {
        let body =
            json!({"model": MODEL, "messages": [{"role": "user", "content": "ping"}]}).to_string();
        let request = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: llmshim\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        self.stream.write_all(request.as_bytes()).await.unwrap();
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let read = self.stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the connection closed before a response");
            seen.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&seen[..end]).to_lowercase();
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        if let Some(length) = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(|value| value.trim().parse::<usize>().unwrap())
        {
            while seen.len() < end + length {
                let read = self.stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the connection closed inside a response");
                seen.extend_from_slice(&chunk[..read]);
            }
        } else {
            assert!(
                head.contains("transfer-encoding: chunked"),
                "unframed response: {head}"
            );
            while !seen.ends_with(b"0\r\n\r\n") {
                let read = self.stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the connection closed inside a response");
                seen.extend_from_slice(&chunk[..read]);
            }
        }
        status
    }
}

async fn applied(server: &Server, generation: u64) -> Value {
    wait_for_reload(server, &format!("generation {generation}"), |status| {
        status["generation"] == generation
    })
    .await
}

async fn mock_upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(calls)
        .create_async()
        .await;
    (upstream, mock)
}

#[tokio::test]
async fn a_revoked_key_is_refused_on_the_next_request_of_an_open_connection() {
    let (upstream, mock) = mock_upstream(3).await;
    let url = upstream.url();
    let first = render(&url, "", &key("second", "second", SECOND, ""));
    let server = start(&first, "reload-revoke", &[]);
    let mut connection = Raw::connect(&server).await;
    assert_eq!(connection.status(SECOND).await, 200);
    assert_eq!(connection.status(THIRD).await, 401);

    // Revoke SECOND and grant THIRD, in one edit.
    rewrite(
        &server,
        &render(&url, "", &key("third", "third", THIRD, "")),
    );
    hangup(&server);
    let status = applied(&server, 1).await;
    assert_eq!(status["last_reload"]["outcome"], "applied");

    // The same connection, the very next requests.
    assert_eq!(connection.status(SECOND).await, 401, "revoked");
    assert_eq!(connection.status(THIRD).await, 200, "granted");
    // A key the file never touched keeps working.
    assert_eq!(connection.status(CALLER_KEY).await, 200);
    mock.assert_async().await;
}

#[tokio::test]
async fn a_file_that_does_not_load_changes_nothing_and_echoes_no_secret() {
    let (upstream, _mock) = mock_upstream(2).await;
    let url = upstream.url();
    let good = render(&url, "", &key("second", "second", SECOND, ""));
    let server = start(&good, "reload-broken", &[]);

    // Half a key, as a file caught mid-write looks.
    rewrite(&server, &format!("{good}\n[[keys]]\nname = "));
    hangup(&server);
    let status = wait_for_reload(&server, "a rejection", |status| {
        status["last_reload"]["outcome"] == "rejected"
    })
    .await;
    assert_eq!(status["generation"], 0);
    assert_eq!(status["last_reload"]["issues"][0]["path"], "(file)");
    let shown = status.to_string();
    for secret in [
        CALLER_KEY,
        SECOND,
        UPSTREAM_KEY,
        ADMIN_TOKEN,
        digest(SECOND).as_str(),
    ] {
        assert!(!shown.contains(secret), "{shown}");
    }
    // The previous keys keep serving.
    let response = chat(&server, Some(SECOND), MODEL).await;
    assert_eq!(response.status(), 200);

    // Putting the file right is applied, and clears nothing it should not.
    rewrite(
        &server,
        &render(&url, "", &key("third", "third", THIRD, "")),
    );
    hangup(&server);
    let status = applied(&server, 1).await;
    assert_eq!(status["last_reload"]["outcome"], "applied");
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 401);
}

#[tokio::test]
async fn a_change_outside_the_keys_is_refused_by_path_and_takes_nothing_with_it() {
    let (upstream, _mock) = mock_upstream(1).await;
    let url = upstream.url();
    let good = render(&url, "", &key("second", "second", SECOND, ""));
    let server = start(&good, "reload-restart-only", &[]);

    // A limits change and a new key in one edit: the key must not slip through.
    rewrite(
        &server,
        &render(
            &url,
            "[limits]\nrpm = 600",
            &format!(
                "{}{}",
                key("second", "second", SECOND, ""),
                key("third", "third", THIRD, "")
            ),
        ),
    );
    hangup(&server);
    let status = wait_for_reload(&server, "a rejection", |status| {
        status["last_reload"]["outcome"] == "rejected"
    })
    .await;
    assert_eq!(status["generation"], 0);
    let issues = status["last_reload"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{status}");
    assert_eq!(issues[0]["path"], "limits");
    assert!(issues[0]["message"]
        .as_str()
        .unwrap()
        .contains("restart to apply"));
    assert!(!status.to_string().contains("600"), "no value is reported");
    assert_eq!(chat(&server, Some(THIRD), MODEL).await.status(), 401);
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 200);
}

#[tokio::test]
async fn a_reload_is_not_a_limit_reset_and_a_changed_limit_reaches_the_existing_bucket() {
    let (upstream, _mock) = mock_upstream(2).await;
    let url = upstream.url();
    // Two requests per minute: the third is shed.
    let limited = |rpm: u32| key("second", "second", SECOND, &format!("rpm = {rpm}"));
    let server = start(&render(&url, "", &limited(2)), "reload-limits", &[]);
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 200);
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 200);
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 429);

    // An unrelated reload (a new key) must not refill the bucket.
    rewrite(
        &server,
        &render(
            &url,
            "",
            &format!("{}{}", limited(2), key("third", "third", THIRD, "")),
        ),
    );
    hangup(&server);
    applied(&server, 1).await;
    assert_eq!(
        chat(&server, Some(SECOND), MODEL).await.status(),
        429,
        "the reload reset the tenant's limit"
    );

    // Raising the limit reaches the bucket that already exists: it refills at the
    // new rate (20 a second) rather than waiting out the old one (30 seconds).
    rewrite(
        &server,
        &render(
            &url,
            "",
            &format!("{}{}", limited(1200), key("third", "third", THIRD, "")),
        ),
    );
    hangup(&server);
    applied(&server, 2).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if chat(&server, Some(SECOND), MODEL).await.status() == 200 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the raised limit never took effect"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn the_file_is_watched_without_a_signal() {
    let (upstream, _mock) = mock_upstream(1).await;
    let url = upstream.url();
    let top = "[reload]\ninterval_secs = 1";
    let server = start(
        &render(&url, top, &key("second", "second", SECOND, "")),
        "reload-poll",
        &[],
    );
    rewrite(
        &server,
        &render(&url, top, &key("third", "third", THIRD, "")),
    );
    // No SIGHUP: the timer finds it.
    applied(&server, 1).await;
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 401);
    assert_eq!(chat(&server, Some(THIRD), MODEL).await.status(), 200);
}

#[tokio::test]
async fn with_polling_off_only_a_signal_reloads() {
    let (upstream, _mock) = mock_upstream(0).await;
    let url = upstream.url();
    let top = "[reload]\nwatch = false";
    let server = start(
        &render(&url, top, &key("second", "second", SECOND, "")),
        "reload-signal-only",
        &[],
    );
    rewrite(
        &server,
        &render(&url, top, &key("third", "third", THIRD, "")),
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(reload_status(&server).await["generation"], 0);
    hangup(&server);
    applied(&server, 1).await;
}

#[tokio::test]
async fn usage_records_name_a_tenant_that_a_reload_added() {
    let (upstream, _mock) = mock_upstream(2).await;
    let url = upstream.url();
    let directory = tempdir::Dir::new("reload-usage");
    let log = directory.0.join("usage.jsonl");
    let top = format!("[logging]\nusage_log = \"{}\"", log.display());
    let server = start(
        &render(&url, &top, &key("second", "first-tenant", SECOND, "")),
        "reload-usage",
        &[],
    );
    assert_eq!(chat(&server, Some(SECOND), MODEL).await.status(), 200);

    rewrite(
        &server,
        &render(&url, &top, &key("third", "added-tenant", THIRD, "")),
    );
    hangup(&server);
    applied(&server, 1).await;
    assert_eq!(chat(&server, Some(THIRD), MODEL).await.status(), 200);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let tenants = loop {
        let tenants: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["tenant"].clone())
            .collect();
        if tenants.len() == 2 || std::time::Instant::now() > deadline {
            break tenants;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(tenants, vec![json!("first-tenant"), json!("added-tenant")]);
}

#[tokio::test]
async fn the_status_is_behind_the_admin_token_and_reports_no_secret() {
    let (upstream, _mock) = mock_upstream(0).await;
    let url = upstream.url();
    let server = start(
        &render(&url, "", &key("second", "second", SECOND, "")),
        "reload-status",
        &[],
    );
    let admin = server.admin.clone().unwrap();
    let client = reqwest::Client::new();
    let url = format!("{admin}/v1/serve/config");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(CALLER_KEY)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Not served on the listener callers use.
    let on_main = client
        .get(format!("{}/v1/serve/config", server.base))
        .bearer_auth(CALLER_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(on_main.status(), 404);

    let status = reload_status(&server).await;
    assert_eq!(status["generation"], 0);
    assert_eq!(
        status["credentials"], 2,
        "the file's two keys: ci and second"
    );
    assert_eq!(status["config_sha256"].as_str().unwrap().len(), 64);
    assert!(status["last_reload"].is_null());
    let shown = status.to_string();
    for secret in [CALLER_KEY, SECOND, UPSTREAM_KEY, ADMIN_TOKEN] {
        assert!(!shown.contains(secret), "{shown}");
    }
}

#[test]
fn a_reload_interval_out_of_range_fails_the_check() {
    let directory = tempdir::Dir::new("reload-config");
    let path = directory.0.join("gateway.toml");
    let text = config("http://127.0.0.1:1", "[reload]\ninterval_secs = 0")
        .replace("{DIR}", directory.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap(), "--check"])
        .env_clear()
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .env("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("reload.interval_secs: must be between 1 and 86400"),
        "{stderr}"
    );
}
