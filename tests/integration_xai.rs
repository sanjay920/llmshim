/// Integration tests for xAI (Grok) hitting the real API.
/// Run with: XAI_API_KEY=... cargo test --test integration_xai -- --ignored --nocapture
use serde_json::json;

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

#[tokio::test]
#[ignore]
async fn grok_4_6_completion() {
    if std::env::var("XAI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "xai/grok-4.6",
        "messages": [{"role": "user", "content": "In one short sentence, what is Rust?"}],
        "max_tokens": 2000,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected a response, got: {resp}");
    println!("grok-4.6 said: {content}");
}

#[tokio::test]
#[ignore]
async fn grok_4_6_reasoning_none_clamps_to_low() {
    // grok-4.6 400s on reasoning_effort "none"; llmshim must clamp it to "low".
    // This asserts the request SUCCEEDS (not a 400), proving the clamp.
    if std::env::var("XAI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "xai/grok-4.6",
        "messages": [{"role": "user", "content": "Say hi."}],
        "max_tokens": 2000,
        "reasoning_effort": "none",
    });
    let resp = llmshim::completion(&router, &req)
        .await
        .expect("reasoning_effort=none must be clamped to low, not 400");
    assert_eq!(resp["object"], "chat.completion");
    println!("grok-4.6 none->low clamp OK");
}

#[tokio::test]
#[ignore]
async fn grok_4_6_reasoning_high() {
    if std::env::var("XAI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "xai/grok-4.6",
        "messages": [{"role": "user", "content": "What is 17 * 24? Reason it through."}],
        "max_tokens": 3000,
        "reasoning_effort": "high",
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected an answer, got: {resp}");
    assert!(content.contains("408"), "expected 408 in answer: {content}");
    println!("grok-4.6 reasoning OK: {content}");
}
