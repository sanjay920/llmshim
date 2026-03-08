/// Integration tests for retry + fallback chains.
/// Run with: cargo test --test integration_fallback -- --ignored
use llmshim::fallback::FallbackConfig;
use serde_json::json;
use std::time::Duration;

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

// ============================================================
// Basic fallback: bad model → good model
// ============================================================

#[tokio::test]
#[ignore]
async fn fallback_bad_model_to_good_model() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let config = FallbackConfig::new(vec![
        "anthropic/nonexistent-model-xyz".into(), // will fail (404)
        "anthropic/claude-sonnet-4-6".into(),     // will succeed
    ])
    .max_retries(0); // don't retry, just fall through

    let request = json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "Say 'pong'. Just that word."}],
        "max_tokens": 100,
    });

    let resp = llmshim::completion_with_fallback(&router, &request, &config, None)
        .await
        .unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    assert!(content.contains("pong"), "Expected pong, got: {}", content);
}

// ============================================================
// Fallback across providers
// ============================================================

#[tokio::test]
#[ignore]
async fn fallback_across_providers() {
    if std::env::var("OPENAI_API_KEY").is_err() || std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let config = FallbackConfig::new(vec![
        "openai/nonexistent-model".into(),    // will fail
        "anthropic/nonexistent-model".into(), // will fail
        "anthropic/claude-sonnet-4-6".into(), // will succeed
    ])
    .max_retries(0);

    let request = json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "Say 'hello'. Just that word."}],
        "max_tokens": 100,
    });

    let resp = llmshim::completion_with_fallback(&router, &request, &config, None)
        .await
        .unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    assert!(content.contains("hello"), "Got: {}", content);
}

// ============================================================
// Primary model succeeds — no fallback needed
// ============================================================

#[tokio::test]
#[ignore]
async fn fallback_primary_succeeds() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let config = FallbackConfig::new(vec![
        "anthropic/claude-sonnet-4-6".into(), // primary — should succeed
        "openai/gpt-5.4".into(),              // fallback — should NOT be called
    ]);

    let request = json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "Say 'first'. Just that word."}],
        "max_tokens": 100,
    });

    let resp = llmshim::completion_with_fallback(&router, &request, &config, None)
        .await
        .unwrap();
    // Should be from Anthropic, not OpenAI
    assert!(
        resp["id"].as_str().unwrap().starts_with("msg_"),
        "Expected Anthropic response (msg_*), got: {}",
        resp["id"]
    );
}

// ============================================================
// All models fail → AllFailed error
// ============================================================

#[tokio::test]
#[ignore]
async fn fallback_all_fail() {
    let router = router();
    let config = FallbackConfig::new(vec![
        "openai/nonexistent-1".into(),
        "anthropic/nonexistent-2".into(),
        "gemini/nonexistent-3".into(),
    ])
    .max_retries(0);

    let request = json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 100,
    });

    let result = llmshim::completion_with_fallback(&router, &request, &config, None).await;
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("all providers failed"), "Got: {}", err);
}

// ============================================================
// Retry with backoff (simulated via a real 429 or bad model)
// ============================================================

#[tokio::test]
#[ignore]
async fn fallback_with_retries() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    // Use a bad model with retries — should exhaust retries then move to fallback
    let config = FallbackConfig::new(vec![
        "anthropic/nonexistent-model".into(), // will fail on every retry
        "anthropic/claude-sonnet-4-6".into(), // fallback
    ])
    .max_retries(1)
    .initial_backoff(Duration::from_millis(100));

    let request = json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "Say 'retried'. Just that word."}],
        "max_tokens": 100,
    });

    let start = std::time::Instant::now();
    let resp = llmshim::completion_with_fallback(&router, &request, &config, None)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    assert!(content.contains("retried"), "Got: {}", content);

    // Should have taken a little longer due to backoff
    println!("Elapsed with retry: {:?}", elapsed);
}

// ============================================================
// Top-level completion still works (no fallback)
// ============================================================

#[tokio::test]
#[ignore]
async fn regular_completion_still_works() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let request = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "Say ok."}],
        "max_tokens": 100,
    });

    // Regular completion (no fallback) should still work
    let resp = llmshim::completion(&router, &request).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
}
