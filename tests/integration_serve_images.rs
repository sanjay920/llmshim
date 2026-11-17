//! `POST /v1/images/generations` on `serve` (note 0036): OpenAI-shaped (`b64_json`), admitted as an
//! attempt by the same key, allowlist, rate and usage rules as a completion, from a real process.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{start, tempdir, ADMIN_TOKEN, CALLER_KEY};
use serde_json::{json, Value};
use std::time::Duration;

fn config(upstream: &str, key_extra: &str, log: Option<&std::path::Path>) -> String {
    let logging = log
        .map(|path| format!("[logging]\nusage_log = \"{}\"\n", path.display()))
        .unwrap_or_default();
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
        {key_extra}
        [admin]
        port = 0
        token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
        {logging}
        "#
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/images/generations")
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "created": 1,
                "data": [{"b64_json": "AQID", "revised_prompt": "a tidy cat"}],
                "usage": {
                    "input_tokens": 5,
                    "input_tokens_details": {"text_tokens": 5, "image_tokens": 0},
                    "output_tokens": 100,
                    "output_tokens_details": {"image_tokens": 100}
                }
            })
            .to_string(),
        )
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

async fn generate(base: &str, key: Option<&str>, body: Value) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

async fn records_of(log: &std::path::Path, at_least: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let found: Vec<Value> = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if found.len() >= at_least || std::time::Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_comes_back_in_openais_shape_and_the_attempt_is_recorded_with_its_price() {
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("img-shape");
    let log = dir.0.join("usage.jsonl");
    let server = start(&config(&up.url(), "", Some(&log)), "img-shape", &env());
    let answer = generate(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "openai/gpt-image-1", "prompt": "a cat", "size": "1024x1024"}),
    )
    .await;
    assert_eq!(answer.status(), 200);
    let body: Value = answer.json().await.unwrap();
    mock.assert_async().await;
    assert_eq!(body["data"][0]["b64_json"], "AQID");
    assert_eq!(body["data"][0]["revised_prompt"], "a tidy cat");
    assert_eq!(body["output_format"], "png");
    assert_eq!(body["usage"]["output_tokens"], 100);
    let records = records_of(&log, 1).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["provider"], "openai");
    assert_eq!(records[0]["outcome"], "completed");
    assert_eq!(records[0]["input_tokens"], 5, "{records:?}");
    assert_eq!(records[0]["output_tokens"], 100, "{records:?}");
    // 5 text tokens at $5/M and 100 image tokens at $40/M: the image response's own price, not the
    // generic per-direction stamp.
    let cost = records[0]["cost_usd"].as_f64().expect("a priced model");
    assert!((cost - 0.004025).abs() < 1e-9, "{cost}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_requests_are_400_before_the_provider() {
    let (up, mock) = upstream(0).await;
    let server = start(&config(&up.url(), "", None), "img-bad", &env());
    for bad in [
        json!({"model": "openai/gpt-image-1", "prompt": "a cat", "response_format": "url"}),
        json!({"model": "openai/gpt-image-1", "prompt": "   "}),
        json!({"model": "openai/gpt-image-1", "prompt": "a cat", "n": 99}),
        // Not an image model: the provider adapter refuses it by name.
        json!({"model": "openai/gpt-6-luna", "prompt": "a cat"}),
        json!({"model": "route/anything", "prompt": "a cat"}),
        json!({"model": "deployment/anything", "prompt": "a cat"}),
    ] {
        let answer = generate(&server.base, Some(CALLER_KEY), bad.clone()).await;
        assert_eq!(answer.status(), 400, "{bad}");
    }
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_allowlist_and_rate_limits_apply() {
    let (up, mock) = upstream(0).await;
    let body = json!({"model": "openai/gpt-image-1", "prompt": "a cat"});
    let listed = start(
        &config(&up.url(), "allowed_models = [\"openai/gpt-6-luna\"]", None),
        "img-auth",
        &env(),
    );
    assert_eq!(
        generate(&listed.base, None, body.clone()).await.status(),
        401
    );
    let refused = generate(&listed.base, Some(CALLER_KEY), body.clone()).await;
    assert_eq!(refused.status(), 403);
    assert!(refused.text().await.unwrap().contains("model_not_allowed"));
    let limited = start(&config(&up.url(), "rpm = 0", None), "img-rpm", &env());
    assert_eq!(
        generate(&limited.base, Some(CALLER_KEY), body)
            .await
            .status(),
        429
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_with_a_spend_cap_cannot_generate_images_even_with_the_opt_in() {
    // The output's size is the model's choice, so there is no enforceable bound, and an unbounded
    // attempt would hold the key's whole allowance until a provider bill that never comes.
    for extra in [
        "budget_usd = 5.0",
        "budget_usd = 5.0\nbudget_allow_unpriced = true",
    ] {
        let (up, mock) = upstream(0).await;
        let server = start(&config(&up.url(), extra, None), "img-budget", &env());
        let answer = generate(
            &server.base,
            Some(CALLER_KEY),
            json!({"model": "openai/gpt-image-1", "prompt": "a cat"}),
        )
        .await;
        assert_eq!(answer.status(), 400, "{extra}");
        assert!(answer
            .text()
            .await
            .unwrap()
            .contains("unpriceable_under_budget"));
        mock.assert_async().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_that_does_not_classify_its_tokens_is_not_priced_at_the_generic_rate() {
    // Without the text/image split of the tokens the catalog's single per-direction rate cannot be
    // applied honestly: the image response's own rule leaves the price null, and the record must
    // agree rather than carry a figure the generic stamp would invent.
    let mut up = mockito::Server::new_async().await;
    let _mock = up
        .mock("POST", "/images/generations")
        .with_header("content-type", "application/json")
        .with_body(
            json!({"data": [{"b64_json": "AQID"}],
                   "usage": {"input_tokens": 5, "output_tokens": 100}})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let dir = tempdir::Dir::new("img-unclassified");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &config(&up.url(), "", Some(&log)),
        "img-unclassified",
        &env(),
    );
    let answer = generate(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "openai/gpt-image-1", "prompt": "a cat"}),
    )
    .await;
    assert_eq!(answer.status(), 200);
    let body: Value = answer.json().await.unwrap();
    assert!(body["usage"]["cost_usd"].is_null(), "{body}");
    let records = records_of(&log, 1).await;
    assert!(records[0]["cost_usd"].is_null(), "{records:?}");
}
