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
