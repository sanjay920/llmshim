//! `POST /v1/audio/transcriptions` on `serve` (note 0038): OpenAI's multipart upload, admitted as an
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
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "text": "hello there",
                "usage": {
                    "type": "tokens",
                    "input_tokens": 14,
                    "input_token_details": {"text_tokens": 0, "audio_tokens": 14},
                    "output_tokens": 5,
                    "total_tokens": 19
                }
            })
            .to_string(),
        )
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

/// The form an OpenAI SDK sends: `file` plus string fields.
fn form(model: &str, audio: &[u8], extra: &[(&str, &str)]) -> reqwest::multipart::Form {
    let part = reqwest::multipart::Part::bytes(audio.to_vec())
        .file_name("clip.wav")
        .mime_str("audio/wav")
        .unwrap();
    let mut form = reqwest::multipart::Form::new()
        .text("model", model.to_owned())
        .part("file", part);
    for (key, value) in extra {
        form = form.text((*key).to_owned(), (*value).to_owned());
    }
    form
}

async fn transcribe(
    base: &str,
    key: Option<&str>,
    form: reqwest::multipart::Form,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form);
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

const MODEL: &str = "openai/gpt-4o-transcribe";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transcript_comes_back_and_the_attempt_is_recorded_with_the_providers_counters_and_price()
{
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("tr-shape");
    let log = dir.0.join("usage.jsonl");
    let server = start(&config(&up.url(), "", Some(&log)), "tr-shape", &env());
    let answer = transcribe(
        &server.base,
        Some(CALLER_KEY),
        form(MODEL, b"RIFF....WAVEfmt ", &[("language", "en")]),
    )
    .await;
    assert_eq!(answer.status(), 200);
    let body: Value = answer.json().await.unwrap();
    mock.assert_async().await;
    assert_eq!(body["text"], "hello there");
    let records = records_of(&log, 1).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["provider"], "openai");
    assert_eq!(records[0]["outcome"], "completed");
    assert_eq!(records[0]["input_tokens"], 14, "{records:?}");
    assert_eq!(records[0]["output_tokens"], 5, "{records:?}");
    // 14 audio tokens at $6/M plus 5 output tokens at $10/M.
    let cost = records[0]["cost_usd"].as_f64().expect("priced");
    assert!((cost - 0.000134).abs() < 1e-12, "{cost}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_format_answers_plain_text() {
    let mut up = mockito::Server::new_async().await;
    let _mock = up
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "text/plain")
        .with_body("hello there")
        .expect(1)
        .create_async()
        .await;
    let server = start(&config(&up.url(), "", None), "tr-text", &env());
    let answer = transcribe(
        &server.base,
        Some(CALLER_KEY),
        form(
            "openai/whisper-1",
            b"RIFF....WAVEfmt ",
            &[("response_format", "text")],
        ),
    )
    .await;
    assert_eq!(answer.status(), 200);
    assert!(answer.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    assert_eq!(answer.text().await.unwrap(), "hello there");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_uploads_are_refused_before_the_provider() {
    let (up, mock) = upstream(0).await;
    let server = start(&config(&up.url(), "", None), "tr-bad", &env());
    // Not multipart, no file, no model, a model that does not transcribe, a bad language, an empty
    // file, a route, an unknown field's value that is not text.
    let json_body = reqwest::Client::new()
        .post(format!("{}/v1/audio/transcriptions", server.base))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": MODEL}))
        .send()
        .await
        .unwrap();
    assert_eq!(json_body.status(), 400);
    let no_file = reqwest::multipart::Form::new().text("model", MODEL);
    assert_eq!(
        transcribe(&server.base, Some(CALLER_KEY), no_file)
            .await
            .status(),
        400
    );
    let no_model = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(b"abc".to_vec()).file_name("a.wav"),
    );
    assert_eq!(
        transcribe(&server.base, Some(CALLER_KEY), no_model)
            .await
            .status(),
        400
    );
    for bad in [
        form("openai/gpt-6-luna", b"abc", &[]),
        form(MODEL, b"abc", &[("language", "english")]),
        form(MODEL, b"", &[]),
        form("route/x", b"abc", &[]),
        form(MODEL, b"abc", &[("response_format", "verbose_json")]),
        form(MODEL, b"abc", &[("temperature", "warm")]),
    ] {
        let answer = transcribe(&server.base, Some(CALLER_KEY), bad).await;
        assert_eq!(answer.status(), 400);
    }
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_over_the_limit_is_413_before_it_is_queued() {
    let (up, mock) = upstream(0).await;
    let server = start(&config(&up.url(), "", None), "tr-big", &env());
    let big = vec![0u8; llmshim::audio::MAX_TRANSCRIPTION_BYTES + 1];
    let answer = transcribe(&server.base, Some(CALLER_KEY), form(MODEL, &big, &[])).await;
    assert_eq!(answer.status(), 413);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_allowlist_and_rate_limits_apply() {
    let (up, mock) = upstream(0).await;
    let listed = start(
        &config(&up.url(), "allowed_models = [\"openai/whisper-1\"]", None),
        "tr-auth",
        &env(),
    );
    assert_eq!(
        transcribe(&listed.base, None, form(MODEL, b"abc", &[]))
            .await
            .status(),
        401
    );
    let refused = transcribe(&listed.base, Some(CALLER_KEY), form(MODEL, b"abc", &[])).await;
    assert_eq!(refused.status(), 403);
    assert!(refused.text().await.unwrap().contains("model_not_allowed"));
    let limited = start(&config(&up.url(), "rpm = 0", None), "tr-rpm", &env());
    assert_eq!(
        transcribe(&limited.base, Some(CALLER_KEY), form(MODEL, b"abc", &[]))
            .await
            .status(),
        429
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_with_a_spend_cap_cannot_transcribe_even_with_the_opt_in() {
    // The cost follows the audio's length, which its compressed bytes do not bound.
    for extra in [
        "budget_usd = 5.0",
        "budget_usd = 5.0\nbudget_allow_unpriced = true",
    ] {
        let (up, mock) = upstream(0).await;
        let server = start(&config(&up.url(), extra, None), "tr-budget", &env());
        let answer = transcribe(&server.base, Some(CALLER_KEY), form(MODEL, b"abc", &[])).await;
        assert_eq!(answer.status(), 400, "{extra}");
        assert!(answer
            .text()
            .await
            .unwrap()
            .contains("unpriceable_under_budget"));
        mock.assert_async().await;
    }
}

/// A multipart body written by hand, so a part can omit its Content-Type as `curl -F file=@a.wav`
/// does (a typed client always sets one).
async fn raw_upload(base: &str, file_headers: &str) -> reqwest::Response {
    let boundary = "llmshimtestboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{MODEL}\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\n{file_headers}\r\n\r\nRIFF....WAVEfmt \r\n--{boundary}--\r\n")
            .as_bytes(),
    );
    reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .bearer_auth(CALLER_KEY)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_part_without_a_usable_content_type_is_served_as_untyped_audio() {
    let (up, mock) = upstream(2).await;
    let server = start(&config(&up.url(), "", None), "tr-ctype", &env());
    // No Content-Type on the part: OpenAI accepts it, so it is not a 502 after queueing.
    let served = raw_upload(
        &server.base,
        "Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"",
    )
    .await;
    assert_eq!(served.status(), 200, "{}", served.text().await.unwrap());
    // A type the form parser cannot read is the same as none, not a 502 after queueing.
    let served_too = raw_upload(
        &server.base,
        "Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: not a type",
    )
    .await;
    assert_eq!(
        served_too.status(),
        200,
        "{}",
        served_too.text().await.unwrap()
    );
    mock.assert_async().await;
}
