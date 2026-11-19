//! External guardrail classifiers on `serve` (note 0048), from a real process against a mock
//! classifier: a verdict blocks a request before the provider is reached and an answer after it was
//! paid for, a classifier that is down fails by its rule's policy, a tenant under an answer
//! classifier cannot stream, and the classifier is sent the text and nothing else.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, start, ADMIN_TOKEN, CALLER_KEY};
use mockito::Matcher;
use serde_json::{json, Value};

const TOKEN: &str = "classifier-token-0123456789";

fn config(classifier_url: &str, upstream: &str, rule: &str) -> String {
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.p]
        kind = "openai-compatible"
        base_url = "{upstream}"
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
        [admin]
        port = 0
        token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
        [logging]
        audit_log = "{{DIR}}/audit.jsonl"
        [classifiers.mod]
        url = "{classifier_url}"
        token = {{ env = "CLASSIFIER_TOKEN" }}
        allow_insecure_http = true
        timeout_ms = 1000
        {rule}
        "#
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("CLASSIFIER_TOKEN", TOKEN)]
}

const BOTH: &str =
    "[[guardrails]]\nname = \"moderation\"\nclassifier = \"mod\"\nscope = \"both\"\n";

/// A classifier that blocks any text containing BADWORD and allows the rest, and one that must never
/// be sent the caller's identity.
async fn classifier() -> (mockito::ServerGuard, Vec<mockito::Mock>) {
    let mut server = mockito::Server::new_async().await;
    let leak = server
        .mock("POST", "/secret/path/check")
        .match_body(Matcher::Regex(r"platform|caller-key|\bci\b".into()))
        .with_body(r#"{"verdict":"allow"}"#)
        .expect(0)
        .create_async()
        .await;
    // One mock decides by content: mockito prefers a mock whose expectation is not yet met, so two
    // mocks (one per verdict) would stop blocking after the first block.
    let verdict = server
        .mock("POST", "/secret/path/check")
        .match_header("authorization", format!("Bearer {TOKEN}").as_str())
        .with_body_from_request(|request| {
            let body = String::from_utf8_lossy(request.body().unwrap()).into_owned();
            if body.contains("BADWORD") {
                br#"{"verdict":"block","category":"hate"}"#.to_vec()
            } else {
                br#"{"verdict":"allow"}"#.to_vec()
            }
        })
        .expect_at_least(0)
        .create_async()
        .await;
    (server, vec![leak, verdict])
}

async fn upstream(text: &str, calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion(text))
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

async fn chat(base: &str, content: &str, stream: bool) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": "p/m", "stream": stream,
            "messages": [{"role": "user", "content": content}]}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn metrics(server: &harness::Server) -> String {
    reqwest::Client::new()
        .get(format!("{}/metrics", server.admin.clone().unwrap()))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_request_never_reaches_the_provider_and_the_classifier_sees_only_text() {
    let (classifier, mocks) = classifier().await;
    let (up, upstream_mock) = upstream("pong", 1).await;
    let server = start(
        &config(
            &format!("{}/secret/path/check", classifier.url()),
            &up.url(),
            BOTH,
        ),
        "classifier-request",
        &env(),
    );
    let (status, body) = chat(&server.base, "a clean question", false).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = chat(&server.base, "a BADWORD question", false).await;
    assert_eq!(status, 400, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("guardrail_blocked") && text.contains("moderation"),
        "{text}"
    );
    // The classifier's own words (its category) never reach the caller.
    assert!(
        !text.contains("hate") && !text.contains("BADWORD"),
        "{text}"
    );
    // The clean one reached the provider once; the blocked one never did.
    upstream_mock.assert_async().await;
    // The classifier was never sent the tenant, the key or the key's name.
    mocks[0].assert_async().await;
    let metrics = metrics(&server).await;
    assert!(
        metrics.contains(
            "llmshim_gateway_guardrail_classifier_total{outcome=\"block\",rule=\"moderation\"}"
        ) || metrics.contains(
            "llmshim_gateway_guardrail_classifier_total{rule=\"moderation\",outcome=\"block\"}"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("llmshim_gateway_rejected_total{provider=\"p\",reason=\"guardrail\"} 1"),
        "{metrics}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_answer_was_paid_for_and_is_a_422() {
    let (classifier, _mocks) = classifier().await;
    let (up, upstream_mock) = upstream("here is a BADWORD answer", 1).await;
    let server = start(
        &config(
            &format!("{}/secret/path/check", classifier.url()),
            &up.url(),
            BOTH,
        ),
        "classifier-response",
        &env(),
    );
    let (status, body) = chat(&server.base, "a clean question", false).await;
    assert_eq!(status, 422, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("guardrail_blocked_response") && text.contains("moderation"),
        "{text}"
    );
    assert!(!text.contains("BADWORD"), "{text}");
    upstream_mock.assert_async().await;
    assert!(metrics(&server)
        .await
        .contains("reason=\"guardrail_response\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_classifier_that_is_down_fails_closed_by_default_and_open_when_the_rule_says_so() {
    // Nothing listens here.
    let dead = "http://127.0.0.1:9/check";
    let (up, closed_mock) = upstream("pong", 0).await;
    let closed = start(&config(dead, &up.url(), BOTH), "classifier-closed", &env());
    let (status, body) = chat(&closed.base, "hello", false).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.to_string().contains("guardrail_unavailable"), "{body}");
    closed_mock.assert_async().await;
    assert!(metrics(&closed).await.contains("outcome=\"failed_closed\""));

    let (up, open_mock) = upstream("pong", 1).await;
    let open = start(
        &config(dead, &up.url(), &format!("{BOTH}on_failure = \"allow\"\n")),
        "classifier-open",
        &env(),
    );
    let (status, body) = chat(&open.base, "hello", false).await;
    assert_eq!(status, 200, "{body}");
    open_mock.assert_async().await;
    assert!(metrics(&open).await.contains("outcome=\"failed_open\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tenant_under_an_answer_classifier_cannot_stream_but_a_request_only_rule_leaves_streaming_alone(
) {
    let (classifier, _mocks) = classifier().await;
    let url = format!("{}/secret/path/check", classifier.url());
    let (up, upstream_mock) = upstream("pong", 0).await;
    let server = start(&config(&url, &up.url(), BOTH), "classifier-stream", &env());
    let (status, body) = chat(&server.base, "hello", true).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string().contains("guardrail_stream_refused"),
        "{body}"
    );
    upstream_mock.assert_async().await;

    // `/v1/chat/stream` is always a stream and carries no `stream` flag in its body.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/stream", server.base))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": "p/m", "messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("guardrail_stream_refused"));
    upstream_mock.assert_async().await;

    let request_only = "[[guardrails]]\nname = \"moderation\"\nclassifier = \"mod\"\n";
    let (up, _mock) = upstream("pong", 1).await;
    let free = start(
        &config(&url, &up.url(), request_only),
        "classifier-stream-ok",
        &env(),
    );
    let (status, body) = chat(&free.base, "hello", true).await;
    assert!(
        !body.to_string().contains("guardrail_stream_refused"),
        "{status}: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_audit_record_names_the_rule_and_host_and_never_the_path_or_the_token() {
    let (classifier, _mocks) = classifier().await;
    let (up, _mock) = upstream("pong", 0).await;
    let server = start(
        &config(
            &format!("{}/secret/path/check", classifier.url()),
            &up.url(),
            BOTH,
        ),
        "classifier-audit",
        &env(),
    );
    // The record is written as the listeners come up, so wait for it.
    let mut log = String::new();
    for _ in 0..100 {
        log = std::fs::read_to_string(server.dir.0.join("audit.jsonl")).unwrap_or_default();
        if log.contains('\n') {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let started: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(started["event"], "server.started");
    let rules = &started["detail"]["guardrails"];
    assert_eq!(rules[0]["name"], "moderation");
    assert_eq!(rules[0]["kind"], "classifier");
    assert_eq!(rules[0]["on_failure"], "block");
    assert!(rules[0]["host"].as_str().unwrap().starts_with("127.0.0.1:"));
    assert!(
        !log.contains("secret/path") && !log.contains(TOKEN),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_media_routes_requests_are_judged_too() {
    let (classifier, mocks) = classifier().await;
    let (up, upstream_mock) = upstream("pong", 0).await;
    let server = start(
        &config(
            &format!("{}/secret/path/check", classifier.url()),
            &up.url(),
            BOTH,
        ),
        "classifier-media",
        &env(),
    );
    for (path, body) in [
        (
            "/v1/embeddings",
            json!({"model": "p/m", "input": ["fine", "a BADWORD"]}),
        ),
        (
            "/v1/images/generations",
            json!({"model": "p/m", "prompt": "a BADWORD picture"}),
        ),
        (
            "/v1/audio/speech",
            json!({"model": "p/m", "input": "say a BADWORD", "voice": "alloy"}),
        ),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{}{path}", server.base))
            .bearer_auth(CALLER_KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(status, 400, "{path}: {text}");
        assert!(
            text.contains("guardrail_blocked") && text.contains("moderation"),
            "{path}: {text}"
        );
        assert!(
            !text.contains("BADWORD") && !text.contains("hate"),
            "{path}: {text}"
        );
    }
    upstream_mock.assert_async().await;
    mocks[0].assert_async().await;
}
