//! `POST /v1/audio/speech` on `serve` (note 0037): OpenAI-shaped, the audio as the response body,
//! admitted as an attempt, priced exactly from the request, from a real process.
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

const AUDIO: &[u8] = &[0xff, 0xfb, 0x90, 0x00, 1, 2, 3, 4];

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/audio/speech")
        .with_header("content-type", "audio/mpeg")
        .with_body(AUDIO)
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

async fn speak(base: &str, key: Option<&str>, body: Value) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/audio/speech"))
        .json(&body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

fn hello() -> Value {
    json!({"model": "openai/tts-1", "input": "hello world", "voice": "alloy"})
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
async fn the_audio_is_the_body_and_the_attempt_is_recorded_at_its_exact_price() {
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("speech-shape");
    let log = dir.0.join("usage.jsonl");
    let server = start(&config(&up.url(), "", Some(&log)), "speech-shape", &env());
    let answer = speak(&server.base, Some(CALLER_KEY), hello()).await;
    assert_eq!(answer.status(), 200);
    assert_eq!(
        answer.headers()["content-type"].to_str().unwrap(),
        "audio/mpeg"
    );
    assert_eq!(answer.bytes().await.unwrap().as_ref(), AUDIO);
    mock.assert_async().await;
    let records = records_of(&log, 1).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["provider"], "openai");
    assert_eq!(records[0]["outcome"], "completed");
    // 11 characters at $15 per million characters.
    let cost = records[0]["cost_usd"].as_f64().expect("a priced model");
    assert!((cost - 0.000165).abs() < 1e-12, "{cost}");
    assert_eq!(records[0]["cost_source"], "catalog");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_requests_are_400_before_the_provider() {
    let (up, mock) = upstream(0).await;
    let server = start(&config(&up.url(), "", None), "speech-bad", &env());
    for bad in [
        json!({"model": "openai/tts-1", "input": "  ", "voice": "alloy"}),
        json!({"model": "openai/tts-1", "input": "hi", "voice": "not a word"}),
        json!({"model": "openai/tts-1", "input": "hi", "voice": "alloy", "response_format": "ogg"}),
        json!({"model": "openai/tts-1", "input": "hi", "voice": "alloy", "speed": 9}),
        // Not a speech model: the adapter refuses it by name.
        json!({"model": "openai/gpt-6-luna", "input": "hi", "voice": "alloy"}),
        json!({"model": "route/anything", "input": "hi", "voice": "alloy"}),
    ] {
        let answer = speak(&server.base, Some(CALLER_KEY), bad.clone()).await;
        assert_eq!(answer.status(), 400, "{bad}");
    }
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_allowlist_and_rate_limits_apply() {
    let (up, mock) = upstream(0).await;
    let listed = start(
        &config(&up.url(), "allowed_models = [\"openai/tts-1-hd\"]", None),
        "speech-auth",
        &env(),
    );
    assert_eq!(speak(&listed.base, None, hello()).await.status(), 401);
    let refused = speak(&listed.base, Some(CALLER_KEY), hello()).await;
    assert_eq!(refused.status(), 403);
    assert!(refused.text().await.unwrap().contains("model_not_allowed"));
    let limited = start(&config(&up.url(), "rpm = 0", None), "speech-rpm", &env());
    assert_eq!(
        speak(&limited.base, Some(CALLER_KEY), hello())
            .await
            .status(),
        429
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_key_reserves_the_exact_price_and_is_stopped_when_the_cap_is_spent() {
    // 11 characters cost 165,000 nanodollars. A cap of 400,000 admits two calls (330,000) and
    // refuses the third (495,000): the reservation is the real price, and each call settles at it.
    let (up, mock) = upstream(2).await;
    let server = start(
        &config(&up.url(), "budget_usd = 0.0004", None),
        "speech-cap",
        &env(),
    );
    for call in 0..2 {
        let answer = speak(&server.base, Some(CALLER_KEY), hello()).await;
        assert_eq!(answer.status(), 200, "call {call}");
    }
    assert_eq!(
        speak(&server.base, Some(CALLER_KEY), hello())
            .await
            .status(),
        429
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_request_bigger_than_the_cap_is_refused_before_the_provider() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &config(&up.url(), "budget_usd = 0.0001", None),
        "speech-cap-low",
        &env(),
    );
    // 165,000 nanodollars against a 100,000 cap.
    assert_eq!(
        speak(&server.base, Some(CALLER_KEY), hello())
            .await
            .status(),
        429
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn characters_not_bytes_are_priced_and_the_hd_model_has_its_own_rate() {
    // "héllo wörld" is 11 characters in 13 bytes; tts-1-hd is $30 per million characters.
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("speech-chars");
    let log = dir.0.join("usage.jsonl");
    let server = start(&config(&up.url(), "", Some(&log)), "speech-chars", &env());
    let answer = speak(
        &server.base,
        Some(CALLER_KEY),
        json!({"model": "openai/tts-1-hd", "input": "héllo wörld", "voice": "alloy"}),
    )
    .await;
    assert_eq!(answer.status(), 200);
    mock.assert_async().await;
    let records = records_of(&log, 1).await;
    let cost = records[0]["cost_usd"].as_f64().expect("priced");
    assert!((cost - 0.00033).abs() < 1e-12, "{cost}");
}
