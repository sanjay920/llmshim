//! `llmshim serve` end to end: a real server process, a mock upstream, and real
//! HTTP. Runs offline; the "upstream" is a local mock.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{chat, completion, config, start, tempdir, ADMIN_TOKEN, CALLER_KEY, UPSTREAM_KEY};
use serde_json::{json, Value};
use std::process::Command;

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
    let server = start(&config(&upstream.url(), ""), "ok", &[]);

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
    let server = start(&config(&upstream.url(), ""), "deny", &[]);

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
    let server = start(&config(&upstream.url(), extra), "route", &[]);

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
    let server = start(&config(&upstream.url(), ""), "surface", &[]);
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

#[test]
fn a_configuration_problem_exits_one_without_binding_a_port() {
    let dir = tempdir::Dir::new("bad");
    let path = dir.0.join("bad.toml");
    std::fs::write(&path, "version = 1\n[server]\nhost = \"nope\"\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap()])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("server.host"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

fn admin_config(upstream: &str) -> String {
    config(
        upstream,
        r#"
        [admin]
        port = 0
        token = { env = "SERVE_E2E_ADMIN_TOKEN" }
        "#,
    )
}

#[tokio::test]
async fn operational_endpoints_move_to_their_own_listener() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let server = start(&admin_config(&upstream.url()), "admin", &[]);
    let admin = server.admin.clone().expect("admin listener banner");
    let client = reqwest::Client::new();

    // The main listener no longer exposes any of them, even to a valid caller.
    for path in ["/metrics", "/v1/gateway/stats", "/health", "/ready"] {
        let response = client
            .get(format!("{}{path}", server.base))
            .bearer_auth(CALLER_KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "main listener {path}");
    }
    // Traffic still flows on the main listener.
    let response = chat(&server, Some(CALLER_KEY), "inference/test-model").await;
    assert_eq!(response.status(), 200);
    mock.assert_async().await;

    // Probes are open on the admin listener.
    for path in ["/health", "/ready"] {
        let response = client.get(format!("{admin}{path}")).send().await.unwrap();
        assert_eq!(response.status(), 200, "admin {path}");
    }
    // Metrics and stats need the admin token; no other secret opens them.
    for path in ["/metrics", "/v1/gateway/stats"] {
        let url = format!("{admin}{path}");
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            401,
            "{path}"
        );
        for wrong in [CALLER_KEY, UPSTREAM_KEY, "not-the-admin-token-0123456789"] {
            let response = client.get(&url).bearer_auth(wrong).send().await.unwrap();
            assert_eq!(response.status(), 401, "{path} with a wrong token");
        }
        let response = client
            .get(&url)
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path} with the admin token");
    }
    // The admin listener serves no inference route. Everything but the probes
    // needs the token, so a caller key is refused outright, and even the admin
    // token finds nothing there.
    let inference = format!("{admin}/v1/chat/completions");
    let body = json!({"model": "inference/test-model", "messages": []});
    let with_caller = client.post(&inference).bearer_auth(CALLER_KEY).json(&body);
    assert_eq!(with_caller.send().await.unwrap().status(), 401);
    let with_admin = client.post(&inference).bearer_auth(ADMIN_TOKEN).json(&body);
    assert_eq!(with_admin.send().await.unwrap().status(), 404);
}

#[test]
fn a_taken_admin_port_fails_the_start_cleanly() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let dir = tempdir::Dir::new("taken");
    let path = dir.0.join("gateway.toml");
    let text = config("http://127.0.0.1:1", &format!("[admin]\nport = {port}"))
        .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap()])
        .env_clear()
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .env("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("admin:") && stderr.contains("already in use"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("listening on"),
        "nothing is announced: {stderr}"
    );
}

#[tokio::test]
async fn without_fleet_mode_each_replica_counts_its_own_requests() {
    // The control for the fleet test of the same name: the same key limit, but
    // two independent replicas. Each sees two requests, under the limit of
    // three, so all four succeed. Only a shared store can make the fourth fail.
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(4)
        .create_async()
        .await;
    let text = config(&upstream.url(), "")
        .replace("tenant = \"platform\"", "tenant = \"platform\"\nrpm = 3");
    let a = start(&text, "solo-a", &[]);
    let b = start(&text, "solo-b", &[]);
    for server in [&a, &b, &a, &b] {
        let response = chat(server, Some(CALLER_KEY), "inference/test-model").await;
        assert_eq!(response.status(), 200);
    }
    mock.assert_async().await;
}

fn access_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect()
}

/// The first eight hex characters of the SHA-256 of a token, as the log prints.
fn fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(token.as_bytes()))[..8].to_string()
}

#[tokio::test]
async fn the_access_log_attributes_each_request_without_recording_a_secret() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let dir = tempdir::Dir::new("access-log");
    let log_path = dir.0.join("access.jsonl");
    let extra = format!("[logging]\naccess_log = \"{}\"", log_path.display());
    let server = start(&config(&upstream.url(), &extra), "access", &[]);

    // An accepted request, a request with a wrong key, and one with none.
    let ok = chat(&server, Some(CALLER_KEY), "inference/test-model").await;
    assert_eq!(ok.status(), 200);
    let wrong = "a-wrong-token-0123456789";
    assert_eq!(
        chat(&server, Some(wrong), "inference/test-model")
            .await
            .status(),
        401
    );
    assert_eq!(
        chat(&server, None, "inference/test-model").await.status(),
        401
    );
    // A query string must not reach the log.
    let health = reqwest::Client::new()
        .get(format!("{}/health?secret-in-query=hunter2", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);

    let lines = access_lines(&log_path);
    assert_eq!(lines.len(), 4, "{lines:#?}");

    let accepted = &lines[0];
    assert_eq!(accepted["event"], "access");
    assert_eq!(accepted["status"], 200);
    assert_eq!(accepted["method"], "POST");
    assert_eq!(accepted["path"], "/v1/chat/completions");
    assert_eq!(accepted["tenant"], "platform");
    assert_eq!(accepted["credential"], fingerprint(CALLER_KEY));
    assert!(accepted["request_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert!(accepted["latency_ms"].is_u64());
    assert!(accepted["ts"].as_str().is_some_and(|ts| ts.ends_with('Z')));

    let refused = &lines[1];
    assert_eq!(refused["status"], 401);
    assert!(refused["tenant"].is_null(), "an unknown key has no tenant");
    assert_eq!(refused["credential"], fingerprint(wrong));
    assert!(lines[2]["credential"].is_null() && lines[2]["tenant"].is_null());

    assert_eq!(lines[3]["path"], "/health", "the query string is dropped");

    // Nothing sensitive is anywhere in the file.
    let raw = std::fs::read_to_string(&log_path).unwrap();
    for secret in [CALLER_KEY, UPSTREAM_KEY, wrong, "hunter2", "ping", "pong"] {
        assert!(!raw.contains(secret), "{secret} leaked into the access log");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the log is owner-only");
    }
}

#[test]
fn an_unwritable_access_log_fails_the_start_with_the_setting_named() {
    let dir = tempdir::Dir::new("access-bad");
    let path = dir.0.join("gateway.toml");
    let text = config(
        "http://127.0.0.1:1",
        "[logging]\naccess_log = \"/nonexistent-directory/access.jsonl\"",
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap()])
        .env_clear()
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .env("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("logging.access_log: cannot open"),
        "{stderr}"
    );
    assert!(!stderr.contains("listening on"), "{stderr}");
}

/// A configuration with a hosted, priced provider whose address is the mock, so
/// the catalog can compute a cost.
fn priced_config(upstream: &str, extra: &str) -> String {
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.openai]
        kind = "openai"
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

fn responses_body(text: &str, input_tokens: u64, output_tokens: u64) -> String {
    json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-6-luna",
        "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}],
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens}
    })
    .to_string()
}

#[tokio::test]
async fn the_usage_log_attributes_tokens_and_cost_to_the_tenant() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/responses")
        .with_header("content-type", "application/json")
        .with_body(responses_body("pong", 1000, 500))
        .expect(1)
        .create_async()
        .await;
    let dir = tempdir::Dir::new("usage-log");
    let usage_path = dir.0.join("usage.jsonl");
    let extra = format!("[logging]\nusage_log = \"{}\"", usage_path.display());
    let server = start(&priced_config(&upstream.url(), &extra), "usage", &[]);

    let response = chat(&server, Some(CALLER_KEY), "openai/gpt-6-luna").await;
    assert_eq!(response.status(), 200);

    let lines = access_lines(&usage_path);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let record = &lines[0];
    assert_eq!(record["event"], "usage");
    assert_eq!(record["tenant"], "platform");
    assert_eq!(record["provider"], "openai");
    assert_eq!(record["model"], "gpt-6-luna");
    assert_eq!(record["kind"], "completion");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["input_tokens"], 1000);
    assert_eq!(record["output_tokens"], 500);
    assert_eq!(record["usage_reported"], true);
    // GPT-6 Luna: $0.10 per million input tokens, $0.50 per million output.
    let cost = record["cost_usd"]
        .as_f64()
        .expect("a priced model has a cost");
    assert!((cost - 0.00035).abs() < 1e-9, "cost_usd was {cost}");
    assert_eq!(record["cost_source"], "catalog");
    assert!(record["attempt_id"]
        .as_str()
        .is_some_and(|id| id.len() == 36));

    // No secret, prompt or completion is in the usage log either.
    let raw = std::fs::read_to_string(&usage_path).unwrap();
    for secret in [CALLER_KEY, UPSTREAM_KEY, "ping", "pong"] {
        assert!(!raw.contains(secret), "{secret} leaked into the usage log");
    }
}

#[tokio::test]
async fn every_failed_attempt_is_recorded_with_an_unknown_cost() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/responses")
        .with_status(500)
        .with_body("{\"error\":{\"message\":\"boom\"}}")
        .expect(2)
        .create_async()
        .await;
    let dir = tempdir::Dir::new("usage-failed");
    let usage_path = dir.0.join("usage.jsonl");
    let extra = format!("[logging]\nusage_log = \"{}\"", usage_path.display());
    // One retry and a short backoff, so the test stays quick; the client's own
    // retry settings are the one thing `serve` still reads from the environment.
    let server = start(
        &priced_config(&upstream.url(), &extra),
        "usage-failed",
        &[
            ("LLMSHIM_MAX_RETRIES", "1"),
            ("LLMSHIM_MAX_BACKOFF_SECS", "1"),
        ],
    );

    let response = chat(&server, Some(CALLER_KEY), "openai/gpt-6-luna").await;
    assert!(response.status().is_server_error(), "{}", response.status());
    let lines = access_lines(&usage_path);
    // One retry means two sends, and each send is its own attempt, so two
    // charges the tenant would have borne had the provider billed them.
    assert_eq!(lines.len(), 2, "{lines:#?}");
    mock.assert_async().await;
    for record in &lines {
        assert_eq!(record["outcome"], "http_failure", "{record}");
        assert_eq!(record["http_status"], 500);
        assert_eq!(record["tenant"], "platform");
        assert!(record["cost_usd"].is_null(), "unknown, not free: {record}");
        assert_eq!(record["usage_reported"], false);
    }
}
