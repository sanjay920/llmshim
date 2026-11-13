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
