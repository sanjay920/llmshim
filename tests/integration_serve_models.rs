//! `llmshim serve` limiting a key to a list of models (note 0028): a real server
//! process and a mock upstream that must not be reached by a refused request.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    completion, config, hangup, rewrite, start, wait_for_reload, ADMIN_TOKEN, CALLER_KEY,
};
use serde_json::{json, Value};

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
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

/// The harness file whose key lists `models`, with a route `cheap` onto
/// `inference/other` and an admin listener for reloads.
fn render(upstream: &str, models: &str) -> String {
    config(
        upstream,
        &format!(
            "allowed_models = {models}\n[admin]\nport = 0\n\
             token = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n\
             [routes.cheap]\nmodel = \"inference/other\"\n"
        ),
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

async fn ask(base: &str, path: &str, model: &str, stream: bool) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": model, "stream": stream, "max_tokens": 16,
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
}

async fn status(base: &str, model: &str) -> u16 {
    ask(base, "/v1/chat/completions", model, false)
        .await
        .status()
        .as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listed_model_is_served_and_any_other_is_refused_before_the_upstream() {
    // Exactly one call reaches the provider: the listed model's.
    let (up, mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), r#"["inference/allowed"]"#),
        "models-exact",
        &env(),
    );
    assert_eq!(status(&server.base, "inference/allowed").await, 200);
    let refused = ask(
        &server.base,
        "/v1/chat/completions",
        "inference/forbidden",
        false,
    )
    .await;
    assert_eq!(refused.status(), 403);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_not_allowed", "{body}");
    // The answer names no model and lists nothing.
    let text = body.to_string();
    assert!(!text.contains("inference/"), "{text}");
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_entry_point_applies_the_list_and_streaming_does_too() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &render(&up.url(), r#"["inference/allowed"]"#),
        "models-entry",
        &env(),
    );
    for (path, stream) in [
        ("/v1/chat/completions", false),
        ("/v1/chat/completions", true),
        ("/v1/chat", false),
        ("/v1/chat/stream", true),
    ] {
        let response = ask(&server.base, path, "inference/forbidden", stream).await;
        assert_eq!(response.status(), 403, "{path} stream={stream}");
    }
    // The Responses wire, unary and streaming.
    for stream in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", server.base))
            .bearer_auth(CALLER_KEY)
            .json(&json!({"model": "inference/forbidden", "stream": stream, "input": "ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "responses stream={stream}");
    }
    // The Gemini wire.
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1beta/models/inference/forbidden:generateContent",
            server.base
        ))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403, "gemini wire");
    // The Anthropic wire, streaming too.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", server.base))
        .header("x-api-key", CALLER_KEY)
        .json(
            &json!({"model": "inference/forbidden", "max_tokens": 16, "stream": true,
            "messages": [{"role": "user", "content": "ping"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403, "anthropic stream");
    // The Anthropic wire.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", server.base))
        .header("x-api-key", CALLER_KEY)
        .json(&json!({"model": "inference/forbidden", "max_tokens": 16,
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prefix_admits_its_family_and_not_a_lookalike() {
    let (up, _mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), r#"["inference/fam/*"]"#),
        "models-prefix",
        &env(),
    );
    assert_eq!(status(&server.base, "inference/fam/small").await, 200);
    assert_eq!(status(&server.base, "inference/family/small").await, 403);
    assert_eq!(status(&server.base, "inference/fam").await, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_can_be_allowed_without_the_model_behind_it() {
    let (up, mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), r#"["route/cheap"]"#),
        "models-route",
        &env(),
    );
    // The route runs the model behind it...
    assert_eq!(status(&server.base, "route/cheap").await, 200);
    // ...but naming that model is not allowed by naming the route.
    assert_eq!(status(&server.base, "inference/other").await, 403);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_that_narrows_the_list_refuses_the_next_request() {
    let (up, _mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), r#"["inference/a", "inference/b"]"#),
        "models-reload",
        &env(),
    );
    assert_eq!(status(&server.base, "inference/b").await, 200);
    rewrite(&server, &render(&up.url(), r#"["inference/a"]"#));
    hangup(&server);
    wait_for_reload(&server, "generation 1", |s| s["generation"] == 1).await;
    assert_eq!(status(&server.base, "inference/b").await, 403);
}

#[test]
fn a_file_with_a_bad_list_is_refused_by_path() {
    use std::process::Command;
    for (list, path) in [
        ("[]", "keys[0].allowed_models"),
        (r#"["inference/*/x"]"#, "keys[0].allowed_models[0]"),
        (r#"["*"]"#, "keys[0].allowed_models[0]"),
        (r#"["gpt-6-luna"]"#, "keys[0].allowed_models[0]"),
        (r#"["inference/a b"]"#, "keys[0].allowed_models[0]"),
    ] {
        let dir = harness::tempdir::Dir::new("models-bad");
        let file = dir.0.join("gateway.toml");
        std::fs::write(
            &file,
            render("http://127.0.0.1:1", list).replace("{DIR}", dir.0.to_str().unwrap()),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args(["serve", "--config", file.to_str().unwrap(), "--check"])
            .env_clear()
            .env("SERVE_E2E_UPSTREAM_KEY", "k")
            .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
            .env("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{list}: {text}");
        assert!(text.contains(path), "{list}: {text}");
    }
}
