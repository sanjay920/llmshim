//! Residency on `serve` (note 0039): a key that may use only providers carrying a residency label,
//! enforced through the allowlist on every route, from a real process.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, start, ADMIN_TOKEN, CALLER_KEY};
use serde_json::{json, Value};

fn config(eu: &str, us: &str, plain: &str, key_extra: &str) -> String {
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.eu_a]
        kind = "openai-compatible"
        base_url = "{eu}"
        residency = "eu"
        [providers.us_a]
        kind = "openai-compatible"
        base_url = "{us}"
        residency = "us"
        [providers.plain]
        kind = "openai-compatible"
        base_url = "{plain}"
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
        {key_extra}
        [admin]
        port = 0
        token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
        "#
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

async fn chat_upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

async fn ask(base: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_in_one_residency_reaches_its_providers_only_and_the_others_never_see_the_request() {
    let (eu, eu_mock) = chat_upstream(1).await;
    let (us, us_mock) = chat_upstream(0).await;
    let (plain, plain_mock) = chat_upstream(0).await;
    let server = start(
        &config(
            &eu.url(),
            &us.url(),
            &plain.url(),
            "allowed_residency = [\"eu\"]",
        ),
        "residency-chat",
        &env(),
    );
    assert_eq!(ask(&server.base, "eu_a/m").await.status(), 200);
    for refused in ["us_a/m", "plain/m"] {
        let answer = ask(&server.base, refused).await;
        assert_eq!(answer.status(), 403, "{refused}");
        assert!(answer.text().await.unwrap().contains("model_not_allowed"));
    }
    eu_mock.assert_async().await;
    us_mock.assert_async().await;
    plain_mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn residency_and_an_allowlist_must_both_admit_and_the_media_routes_obey_it_too() {
    let (eu, _eu_mock) = chat_upstream(0).await;
    let (us, us_mock) = chat_upstream(0).await;
    let (plain, _plain_mock) = chat_upstream(0).await;
    // The list names a provider outside the key's residency: it is the intersection that counts.
    let server = start(
        &config(
            &eu.url(),
            &us.url(),
            &plain.url(),
            "allowed_residency = [\"eu\"]\nallowed_models = [\"eu_a/m\", \"us_a/m\"]",
        ),
        "residency-both",
        &env(),
    );
    assert_eq!(ask(&server.base, "us_a/m").await.status(), 403);
    // The embeddings route asks the same allowlist.
    let embed = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", server.base))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": "us_a/m", "input": ["x"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(embed.status(), 403);
    let body: Value = embed.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_not_allowed");
    us_mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_with_no_residency_is_unrestricted() {
    let (eu, _m1) = chat_upstream(0).await;
    let (us, us_mock) = chat_upstream(1).await;
    let (plain, _m3) = chat_upstream(0).await;
    let server = start(
        &config(&eu.url(), &us.url(), &plain.url(), ""),
        "residency-none",
        &env(),
    );
    assert_eq!(ask(&server.base, "us_a/m").await.status(), 200);
    us_mock.assert_async().await;
}
