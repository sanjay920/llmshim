//! `POST /v1/embeddings` on `serve` (note 0035): OpenAI-shaped, admitted by the same key,
//! allowlist, rate and usage rules as a completion, answered from a real process.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{start, tempdir, ADMIN_TOKEN, CALLER_KEY, UPSTREAM_KEY};
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
        [providers.embed]
        kind = "openai-compatible"
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
        .mock("POST", "/embeddings")
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "object": "list",
                "model": "embed-model-2026",
                "data": [
                    {"object": "embedding", "index": 1, "embedding": [0.5, 0.25]},
                    {"object": "embedding", "index": 0, "embedding": [1.0, -2.0]}
                ],
                "usage": {"prompt_tokens": 7, "total_tokens": 7}
            })
            .to_string(),
        )
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

async fn embed(base: &str, key: Option<&str>, body: Value) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vectors_come_back_in_openais_shape_in_input_order_and_the_attempt_is_recorded() {
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("emb-shape");
    let log = dir.0.join("usage.jsonl");
    let server = start(&config(&up.url(), "", Some(&log)), "emb-shape", &env());
    let answer = embed(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "embed/embed-model", "input": ["first", "second"]}),
    )
    .await;
    assert_eq!(answer.status(), 200);
    let body: Value = answer.json().await.unwrap();
    mock.assert_async().await;
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["index"], 0);
    assert_eq!(body["data"][0]["embedding"], json!([1.0, -2.0]));
    assert_eq!(body["data"][1]["embedding"], json!([0.5, 0.25]));
    assert_eq!(body["model"], "embed-model-2026");
    assert_eq!(body["usage"]["prompt_tokens"], 7);
    // Unknown price is null, never zero.
    assert!(body["usage"]["cost_usd"].is_null(), "{body}");
    // The attempt is a usage record under the key's tenant and the provider's name.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let records = loop {
        let found: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if !found.is_empty() || std::time::Instant::now() > deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["provider"], "embed");
    assert_eq!(records[0]["outcome"], "completed");
    assert_eq!(records[0]["input_tokens"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_string_and_base64_are_accepted_and_bad_shapes_are_400() {
    let (up, _mock) = upstream(0).await;
    // One string, base64 output, against a single-vector answer.
    let mut single = mockito::Server::new_async().await;
    let _m = single
        .mock("POST", "/embeddings")
        .with_header("content-type", "application/json")
        .with_body(
            json!({"data": [{"index": 0, "embedding": [1.0, 2.0]}],
                   "usage": {"prompt_tokens": 1}})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let server = start(&config(&single.url(), "", None), "emb-shapes", &env());
    let answer = embed(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "embed/m", "input": "one", "encoding_format": "base64"}),
    )
    .await;
    assert_eq!(answer.status(), 200);
    let body: Value = answer.json().await.unwrap();
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body["data"][0]["embedding"].as_str().unwrap())
        .unwrap();
    let floats: Vec<f32> = bytes
        .chunks(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(floats, [1.0, 2.0]);
    // Token arrays, a bad format, an empty batch: refused before the provider.
    for bad in [
        json!({"model": "route/anything", "input": "x"}),
        json!({"model": "deployment/anything", "input": "x"}),
        json!({"model": "embed/m", "input": [1, 2, 3]}),
        json!({"model": "embed/m", "input": "x", "encoding_format": "hex"}),
        json!({"model": "embed/m", "input": []}),
    ] {
        let answer = embed(&server.base, Some(CALLER_KEY), bad.clone()).await;
        assert_eq!(answer.status(), 400, "{bad}");
    }
    drop(up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_without_a_key_is_401_and_a_key_with_a_list_may_not_embed_an_unlisted_model() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &config(&up.url(), "allowed_models = [\"embed/other\"]", None),
        "emb-auth",
        &env(),
    );
    let body = json!({"model": "embed/embed-model", "input": ["x"]});
    assert_eq!(embed(&server.base, None, body.clone()).await.status(), 401);
    assert_eq!(
        embed(
            &server.base,
            Some("not-the-key-0123456789abcdef"),
            body.clone()
        )
        .await
        .status(),
        401
    );
    let refused = embed(&server.base, Some(CALLER_KEY), body).await;
    assert_eq!(refused.status(), 403);
    let text = refused.text().await.unwrap();
    assert!(text.contains("model_not_allowed"), "{text}");
    // Neither reached the provider.
    mock.assert_async().await;
    let _ = UPSTREAM_KEY;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_tenant_rate_limit_refuses_embeddings_before_the_provider() {
    // The key's own rpm applies to embeddings as it does to chat: a zero limit never admits.
    let (up, mock) = upstream(0).await;
    let server = start(&config(&up.url(), "rpm = 0", None), "emb-rpm", &env());
    let answer = embed(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "embed/embed-model", "input": ["x"]}),
    )
    .await;
    assert_eq!(answer.status(), 429);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_with_a_spend_cap_is_refused_until_embeddings_have_a_spend_bound() {
    // Spend reservation is built for chat wires; an embeddings attempt has no enforceable bound yet
    // (note 0035), so a capped key is refused, not served unmetered. `budget_allow_unpriced` is the
    // explicit opt-in for that.
    let (up, mock) = upstream(0).await;
    let server = start(
        &config(&up.url(), "budget_usd = 5.0", None),
        "emb-budget",
        &env(),
    );
    let answer = embed(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "embed/embed-model", "input": ["x"]}),
    )
    .await;
    assert_eq!(answer.status(), 400);
    let text = answer.text().await.unwrap();
    assert!(text.contains("unpriceable_under_budget"), "{text}");
    mock.assert_async().await;

    let (up, mock) = upstream(0).await;
    let server = start(
        &config(
            &up.url(),
            "budget_usd = 5.0\nbudget_allow_unpriced = true",
            None,
        ),
        "emb-budget-ok",
        &env(),
    );
    let answer = embed(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "embed/embed-model", "input": ["first", "second"]}),
    )
    .await;
    // Even the explicit opt-in does not admit it: an unbounded attempt would hold the key's whole
    // allowance until a provider bill settled it, and embeddings carry none.
    assert_eq!(answer.status(), 400);
    mock.assert_async().await;
}

/// The same file against `kind = "openai"` (the Responses wire, which counts `input_tokens`): an
/// embeddings body counts `prompt_tokens`, and that must still reach the usage record.
fn openai_config(upstream: &str, key_extra: &str, log: &std::path::Path) -> String {
    config(upstream, key_extra, Some(log)).replace(
        "[providers.embed]\n        kind = \"openai-compatible\"",
        "[providers.openai]\n        kind = \"openai\"",
    )
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
async fn the_openai_wire_records_the_prompt_tokens_an_embeddings_body_reports() {
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("emb-openai");
    let log = dir.0.join("usage.jsonl");
    let server = start(&openai_config(&up.url(), "", &log), "emb-openai", &env());
    let answer = embed(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "openai/text-embedding-3-small", "input": ["a", "b"]}),
    )
    .await;
    assert_eq!(answer.status(), 200, "{}", answer.text().await.unwrap());
    mock.assert_async().await;
    let records = records_of(&log, 1).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["input_tokens"], 7, "{records:?}");
    assert_eq!(records[0]["output_tokens"], 0, "{records:?}");
}
