//! Guardrails on `serve` (note 0040): a request whose text matches a rule is refused before
//! anything is reserved or sent, on every route, from a real process.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, start, tempdir, ADMIN_TOKEN, CALLER_KEY};
use serde_json::{json, Value};
use std::time::Duration;

fn config(upstream: &str, guardrails: &str, log: Option<&std::path::Path>) -> String {
    let logging = log
        .map(|path| {
            format!(
                "[logging]\nusage_log = \"{}\"\naccess_log = \"{}\"\n",
                path.display(),
                path.display().to_string().replace("usage", "access")
            )
        })
        .unwrap_or_default();
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
        {logging}
        {guardrails}
        "#
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

const RULE: &str = r#"
    [[guardrails]]
    name = "cloud-keys"
    block_if_matches = ["AKIA[0-9A-Z]{16}"]
"#;
const SECRET: &str = "my key is AKIAABCDEFGHIJKLMNOP";

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

async fn post(base: &str, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .bearer_auth(CALLER_KEY)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_matching_request_is_refused_by_rule_name_and_a_clean_one_is_served() {
    let (up, mock) = chat_upstream(1).await;
    let server = start(&config(&up.url(), RULE, None), "guard-chat", &env());
    let clean = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(clean.status(), 200);
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "p/m", "messages": [{"role": "user", "content": SECRET}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model": "p/m", "stream": true, "messages": [{"role": "user", "content": SECRET}]}),
        ),
        (
            "/v1/messages",
            json!({"model": "p/m", "max_tokens": 5, "system": SECRET,
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "/v1/embeddings",
            json!({"model": "p/m", "input": ["fine", SECRET]}),
        ),
    ] {
        let refused = post(&server.base, path, body.clone()).await;
        let status = refused.status();
        let text = refused.text().await.unwrap();
        assert_eq!(status, 400, "{path}: {text}");
        assert!(
            text.contains("guardrail_blocked") || text.contains("cloud-keys"),
            "{path}: {text}"
        );
        // The rule's name, never the pattern or what matched.
        assert!(!text.contains("AKIA"), "{path}: {text}");
    }
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_request_leaves_no_usage_record_and_is_counted_by_reason() {
    let (up, mock) = chat_upstream(0).await;
    let dir = tempdir::Dir::new("guard-log");
    let log = dir.0.join("usage.jsonl");
    let server = start(&config(&up.url(), RULE, Some(&log)), "guard-log", &env());
    let refused = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": SECRET}]}),
    )
    .await;
    assert_eq!(refused.status(), 400);
    let metrics = reqwest::Client::new()
        .get(format!("{}/metrics", server.admin.clone().unwrap()))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("llmshim_gateway_rejected_total{provider=\"p\",reason=\"guardrail\"} 1"),
        "{metrics}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Nothing was sent, so no attempt was recorded; and the matched text is in no log.
    let usage = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(usage.is_empty(), "{usage}");
    let access = std::fs::read_to_string(dir.0.join("access.jsonl")).unwrap_or_default();
    assert!(!access.contains("AKIA"), "{access}");
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_that_cannot_fire_or_compile_is_refused_at_start_without_echoing_the_pattern() {
    // The only key belongs to `platform`; a rule for another tenant is a configuration error, not a
    // rule that silently never fires.
    let (up, _mock) = chat_upstream(0).await;
    let dir = tempdir::Dir::new("guard-bad");
    let path = dir.0.join("gateway.toml");
    let bad = config(
        &up.url(),
        "[[guardrails]]\nname = \"scoped\"\ntenants = [\"nobody\"]\nblock_if_matches = [\"x\"]\n\
         [[guardrails]]\nname = \"broken\"\nblock_if_matches = [\"(unclosed\"]",
        None,
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, bad).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args([
            "serve",
            "--check",
            "--structure-only",
            "--config",
            path.to_str().unwrap(),
        ])
        .env_clear()
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr).to_string()
        + &String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("guardrails[0].tenants[0]"), "{text}");
    assert!(text.contains("guardrails[1].block_if_matches[0]"), "{text}");
    // The pattern is not echoed.
    assert!(!text.contains("unclosed"), "{text}");
}

const RESPONSE_RULE: &str = r#"
    [[guardrails]]
    name = "no-leak"
    scope = "response"
    block_if_matches = ["AKIA[0-9A-Z]{16}"]
"#;

async fn leaking_upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion(SECRET))
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_that_breaks_a_response_rule_is_replaced_by_a_422_naming_the_rule() {
    let (up, mock) = leaking_upstream(1).await;
    let dir = tempdir::Dir::new("guard-response");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &config(&up.url(), RESPONSE_RULE, Some(&log)),
        "guard-response",
        &env(),
    );
    // The request itself is fine under a response-only rule, so it goes to the provider.
    let answer = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    let status = answer.status();
    let text = answer.text().await.unwrap();
    assert_eq!(status, 422, "{text}");
    assert!(
        text.contains("guardrail_blocked_response") && text.contains("no-leak"),
        "{text}"
    );
    // Naming the rule, never what matched.
    assert!(!text.contains("AKIA"), "{text}");
    mock.assert_async().await;
    // The provider answered and was paid, so the attempt is recorded; the secret is in no log.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let usage = loop {
        let usage = std::fs::read_to_string(&log).unwrap_or_default();
        if !usage.is_empty() || std::time::Instant::now() > deadline {
            break usage;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(usage.contains("\"outcome\":\"completed\""), "{usage}");
    assert!(!usage.contains("AKIA"), "{usage}");
    let metrics = reqwest::Client::new()
        .get(format!("{}/metrics", server.admin.clone().unwrap()))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("reason=\"guardrail_response\"} 1"),
        "{metrics}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_answer_is_returned_a_request_rule_does_not_read_it_and_a_stream_is_refused() {
    // A rule that reads responses leaves a clean answer alone...
    let (up, mock) = chat_upstream(1).await;
    let server = start(
        &config(&up.url(), RESPONSE_RULE, None),
        "guard-response-ok",
        &env(),
    );
    let clean = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(clean.status(), 200);
    // ...but the caller under it cannot stream, since a stream cannot be checked after it starts.
    let streaming = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "stream": true, "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(streaming.status(), 400);
    mock.assert_async().await;

    // A request-only rule (the default scope) does not read the answer: the same leaky answer
    // passes, and streaming is unaffected.
    let (up, mock) = leaking_upstream(1).await;
    let server = start(&config(&up.url(), RULE, None), "guard-request-only", &env());
    let passed = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(passed.status(), 200);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escaped_tool_call_argument_and_every_streaming_route_are_covered() {
    // A tool call's arguments are JSON text: the secret written with an escape is still read.
    let mut up = mockito::Server::new_async().await;
    let _mock = up
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(
            json!({"id": "c", "object": "chat.completion", "created": 1, "model": "m",
                "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{"id": "call_1", "type": "function", "function": {
                        "name": "f", "arguments": "{\"v\":\"\\u0041KIAABCDEFGHIJKLMNOP\"}"}}]}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let server = start(
        &config(&up.url(), RESPONSE_RULE, None),
        "guard-escaped",
        &env(),
    );
    let blocked = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(blocked.status(), 422);
    // Every route that streams is refused to the caller under the rule.
    for (path, body) in [
        (
            "/v1/chat/stream",
            json!({"model": "p/m", "messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "/v1/messages",
            json!({"model": "p/m", "max_tokens": 5, "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "/v1/responses",
            json!({"model": "p/m", "stream": true, "input": "hi"}),
        ),
    ] {
        let refused = post(&server.base, path, body).await;
        assert_eq!(refused.status(), 400, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redaction_rule_sends_the_provider_the_masked_text_not_the_secret() {
    let mut up = mockito::Server::new_async().await;
    // The provider must receive the replacement and never the secret.
    let masked = up
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::Regex(r"my key is \[hidden\]".into()))
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(4)
        .create_async()
        .await;
    let leaked = up
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::Regex("AKIA".into()))
        .with_status(500)
        .expect(0)
        .create_async()
        .await;
    let rule = "[[guardrails]]\nname = \"mask-keys\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]\nreplacement = \"[hidden]\"";
    let server = start(&config(&up.url(), rule, None), "guard-redact", &env());
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "p/m", "messages": [{"role": "user", "content": SECRET}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model": "p/m", "messages": [{"role": "user", "content": [{"type": "text", "text": SECRET}]}]}),
        ),
        // The streaming route: the provider's answer is not a stream here, so only what it was
        // sent is judged (the mock is matched on the masked text).
        (
            "/v1/chat/stream",
            json!({"model": "p/m", "messages": [{"role": "user", "content": SECRET}]}),
        ),
        // A native facade: the Anthropic `system` becomes a message, and it is masked too.
        (
            "/v1/messages",
            json!({"model": "p/m", "max_tokens": 5, "system": SECRET,
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
    ] {
        let answer = post(&server.base, path, body).await;
        let status = answer.status();
        let text = answer.text().await.unwrap();
        assert_eq!(status, 200, "{path}: {text}");
    }
    masked.assert_async().await;
    leaked.assert_async().await;
    let metrics = reqwest::Client::new()
        .get(format!("{}/metrics", server.admin.clone().unwrap()))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("llmshim_gateway_guardrail_redactions_total{rule=\"mask-keys\"} 4"),
        "{metrics}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redaction_and_blocking_compose_and_a_rule_cannot_do_both() {
    // Redact first, then block what is left: the secret is masked, the other pattern still refuses.
    let (up, mock) = chat_upstream(1).await;
    let rules = "[[guardrails]]\nname = \"mask\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]\n\
                 [[guardrails]]\nname = \"no-pw\"\nblock_if_matches = [\"hunter2\"]";
    let server = start(&config(&up.url(), rules, None), "guard-compose", &env());
    let ok = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": SECRET}]}),
    )
    .await;
    assert_eq!(ok.status(), 200);
    let blocked = post(
        &server.base,
        "/v1/chat/completions",
        json!({"model": "p/m", "messages": [{"role": "user", "content": "pw hunter2"}]}),
    )
    .await;
    assert_eq!(blocked.status(), 400);
    mock.assert_async().await;
    // A rule that sets both is refused at start, by path.
    let dir = tempdir::Dir::new("guard-both");
    let path = dir.0.join("gateway.toml");
    let both = config(
        &up.url(),
        "[[guardrails]]\nname = \"both\"\nblock_if_matches = [\"a\"]\nredact_matches = [\"b\"]\n\
         [[guardrails]]\nname = \"resp\"\nscope = \"response\"\nredact_matches = [\"c\"]",
        None,
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, both).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args([
            "serve",
            "--check",
            "--structure-only",
            "--config",
            path.to_str().unwrap(),
        ])
        .env_clear()
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr).to_string()
        + &String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        text.contains("guardrails[0]") && text.contains("use two rules"),
        "{text}"
    );
    assert!(
        text.contains("guardrails[1].scope") && text.contains("request"),
        "{text}"
    );
}
