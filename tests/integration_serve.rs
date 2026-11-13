//! `llmshim serve` end to end: a real server process, a mock upstream, and real
//! HTTP. Runs offline; the "upstream" is a local mock.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    chat, completion, config, run_to_exit, start, tempdir, ADMIN_TOKEN, CALLER_KEY, UPSTREAM_KEY,
};
use serde_json::{json, Value};

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
    let (code, stderr) = run_to_exit(&path, &[]);
    assert_eq!(code, Some(1));
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
    let text = config(
        "http://127.0.0.1:1",
        &format!("[admin]\nport = {port}\nallow_unauthenticated = true"),
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("LLMSHIM_CATALOG_OFFLINE", "1"),
            ("SERVE_E2E_CALLER_KEY", CALLER_KEY),
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("admin:") && stderr.contains("already in use"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("listening on"),
        "nothing is announced: {stderr}"
    );
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
    let server = start(&config(&upstream.url(), ""), "private-dir", &[]);
    let mode = std::fs::metadata(server.dir.0.join("receipts"))
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

#[test]
fn an_admin_listener_without_a_token_must_be_opted_into() {
    let dir = tempdir::Dir::new("tokenless-admin");
    let path = dir.0.join("gateway.toml");
    let text =
        config("http://127.0.0.1:1", "[admin]\nport = 0").replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("SERVE_E2E_CALLER_KEY", CALLER_KEY),
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("admin.token"), "{stderr}");
    assert!(stderr.contains("allow_unauthenticated"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

#[tokio::test]
async fn an_opted_in_open_admin_listener_says_so_and_serves_without_a_token() {
    let upstream = mockito::Server::new_async().await;
    let extra = "[admin]\nport = 0\nallow_unauthenticated = true";
    let server = start(&config(&upstream.url(), extra), "open-admin", &[]);
    assert!(
        server.admin_banner.contains("OPEN"),
        "{}",
        server.admin_banner
    );
    let admin = server.admin.clone().unwrap();
    let response = reqwest::get(format!("{admin}/metrics")).await.unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn a_protected_admin_listener_says_a_token_is_required() {
    let upstream = mockito::Server::new_async().await;
    let server = start(&admin_config(&upstream.url()), "protected-admin", &[]);
    assert!(
        server.admin_banner.contains("token required"),
        "{}",
        server.admin_banner
    );
    assert!(!server.admin_banner.contains("OPEN"));
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

/// The JSON lines in a log file, once `expected` of them have been written. The
/// server queues lines for a writer thread, so a line is not on disk the instant
/// its response returns; this waits for them, then waits a moment longer and
/// reads again so that an unexpected extra line fails the count rather than
/// slipping in afterwards.
fn access_lines(path: &std::path::Path, expected: usize) -> Vec<Value> {
    let read = || -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while read().len() < expected && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    read()
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

    let lines = access_lines(&log_path, 4);
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

#[tokio::test]
async fn a_native_routes_credential_header_is_attributed_in_the_access_log() {
    // Anthropic clients send `x-api-key`. The gateway authenticates it on the
    // native routes, so the log must name the tenant it admitted, and must
    // fingerprint a wrong one rather than call the request anonymous.
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .create_async()
        .await;
    let dir = tempdir::Dir::new("access-native");
    let log_path = dir.0.join("access.jsonl");
    let extra = format!("[logging]\naccess_log = \"{}\"", log_path.display());
    let server = start(&config(&upstream.url(), &extra), "access-native", &[]);
    let body = json!({"model": "inference/test-model", "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}]});
    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", server.base);

    let admitted = client
        .post(&url)
        .header("x-api-key", CALLER_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_ne!(admitted.status(), 401, "the gateway admits this key");
    let wrong = "a-wrong-x-api-key-0123456789";
    let refused = client
        .post(&url)
        .header("x-api-key", wrong)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);

    let lines = access_lines(&log_path, 2);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert_eq!(lines[0]["path"], "/v1/messages");
    assert_eq!(lines[0]["tenant"], "platform", "{lines:#?}");
    assert_eq!(lines[0]["credential"], fingerprint(CALLER_KEY));
    assert!(lines[1]["tenant"].is_null());
    assert_eq!(lines[1]["credential"], fingerprint(wrong));
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
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("LLMSHIM_CATALOG_OFFLINE", "1"),
            ("SERVE_E2E_CALLER_KEY", CALLER_KEY),
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1));
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

    let lines = access_lines(&usage_path, 1);
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
async fn an_open_server_attributes_usage_to_the_anonymous_tenant() {
    // With allow_unauthenticated no key names a tenant, but every request is
    // attributed to "anonymous"; the record must say so, not `null`, which is
    // reserved for a tenant this replica's configuration does not know.
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/responses")
        .with_header("content-type", "application/json")
        .with_body(responses_body("pong", 10, 5))
        .create_async()
        .await;
    let dir = tempdir::Dir::new("usage-open");
    let usage_path = dir.0.join("usage.jsonl");
    let text = format!(
        r#"
        version = 1
        [server]
        port = 0
        allow_unauthenticated = true
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.openai]
        kind = "openai"
        base_url = "{}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [logging]
        usage_log = "{}"
        "#,
        upstream.url(),
        usage_path.display()
    );
    let server = start(&text, "usage-open", &[]);
    let response = chat(&server, None, "openai/gpt-6-luna").await;
    assert_eq!(response.status(), 200);
    let lines = access_lines(&usage_path, 1);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_eq!(lines[0]["tenant"], "anonymous", "{lines:#?}");
}

#[test]
fn two_logs_on_one_file_are_refused_at_startup() {
    let dir = tempdir::Dir::new("shared-log");
    let path = dir.0.join("gateway.toml");
    let shared = dir.0.join("both.jsonl");
    let text = config(
        "http://127.0.0.1:1",
        &format!(
            "[logging]\naccess_log = \"{0}\"\nusage_log = \"{0}\"",
            shared.display()
        ),
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("LLMSHIM_CATALOG_OFFLINE", "1"),
            ("SERVE_E2E_CALLER_KEY", CALLER_KEY),
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("logging.usage_log"), "{stderr}");
    assert!(!shared.exists());
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
    let lines = access_lines(&usage_path, 2);
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

#[test]
fn a_failed_start_does_not_create_the_access_log() {
    // The caller token is too short, which is found after the log used to be
    // opened; the writable path must be left untouched.
    let dir = tempdir::Dir::new("access-not-created");
    let path = dir.0.join("gateway.toml");
    let log = dir.0.join("access.jsonl");
    let text = config(
        "http://127.0.0.1:1",
        &format!("[logging]\naccess_log = \"{}\"", log.display()),
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let (code, stderr) = run_to_exit(
        &path,
        &[
            ("LLMSHIM_CATALOG_OFFLINE", "1"),
            ("SERVE_E2E_CALLER_KEY", "short"),
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(!log.exists(), "a failed start must not create the log");
}
