use llmshim::fallback::FallbackConfig;
use std::time::Duration;

#[test]
fn fallback_config_defaults() {
    let config = FallbackConfig::default();
    assert_eq!(config.max_retries, 2);
    assert_eq!(config.initial_backoff, Duration::from_millis(500));
    assert!(config.retryable_statuses.contains(&429));
    assert!(config.retryable_statuses.contains(&500));
    assert!(config.retryable_statuses.contains(&502));
    assert!(config.retryable_statuses.contains(&503));
    assert!(config.retryable_statuses.contains(&529));
}

#[test]
fn fallback_config_new_with_models() {
    let config = FallbackConfig::new(vec![
        "anthropic/claude-sonnet-4-6".into(),
        "openai/gpt-5.4".into(),
    ]);
    assert_eq!(config.models.len(), 2);
    assert_eq!(config.models[0], "anthropic/claude-sonnet-4-6");
    assert_eq!(config.models[1], "openai/gpt-5.4");
}

#[test]
fn fallback_config_builder() {
    let config = FallbackConfig::new(vec!["a".into()])
        .max_retries(5)
        .initial_backoff(Duration::from_secs(1));
    assert_eq!(config.max_retries, 5);
    assert_eq!(config.initial_backoff, Duration::from_secs(1));
}

#[tokio::test]
async fn fallback_no_models_uses_request_model() {
    // Empty fallback config should just try the model from the request
    let router = llmshim::router::Router::new();
    let config = FallbackConfig::default();
    let request = serde_json::json!({
        "model": "unknown/nonexistent",
        "messages": [{"role": "user", "content": "hi"}],
    });

    let result = llmshim::completion_with_fallback(&router, &request, &config, None).await;
    // Should fail because the provider isn't registered
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("unknown"), "Error: {}", err);
}

#[tokio::test]
async fn fallback_all_bad_models_returns_all_failed() {
    let router = llmshim::router::Router::new();
    let config = FallbackConfig::new(vec![
        "bad/model-1".into(),
        "bad/model-2".into(),
        "bad/model-3".into(),
    ])
    .max_retries(0);

    let request = serde_json::json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "hi"}],
    });

    let result = llmshim::completion_with_fallback(&router, &request, &config, None).await;
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("all providers failed"), "Error: {}", err);
}

#[test]
fn fallback_config_retryable_statuses() {
    let config = FallbackConfig::default();
    // These should be retryable
    assert!(config.retryable_statuses.contains(&429)); // rate limit
    assert!(config.retryable_statuses.contains(&500)); // internal server error
    assert!(config.retryable_statuses.contains(&502)); // bad gateway
    assert!(config.retryable_statuses.contains(&503)); // service unavailable
    assert!(config.retryable_statuses.contains(&529)); // overloaded (Anthropic)
                                                       // These should NOT be retryable
    assert!(!config.retryable_statuses.contains(&400)); // bad request
    assert!(!config.retryable_statuses.contains(&401)); // unauthorized
    assert!(!config.retryable_statuses.contains(&404)); // not found
}

#[tokio::test]
async fn fallback_missing_model_field_errors() {
    let router = llmshim::router::Router::new();
    let config = FallbackConfig::default();
    let request = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
    let result = llmshim::completion_with_fallback(&router, &request, &config, None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn fallback_collects_errors_from_all_models() {
    let router = llmshim::router::Router::new();
    let config = FallbackConfig::new(vec!["bad/one".into(), "bad/two".into()]).max_retries(0);

    let request = serde_json::json!({
        "model": "ignored",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = llmshim::completion_with_fallback(&router, &request, &config, None).await;
    let err = format!("{}", result.unwrap_err());
    // Should mention both models
    assert!(err.contains("one"), "Should mention first model: {}", err);
    assert!(err.contains("two"), "Should mention second model: {}", err);
}

#[test]
fn fallback_config_zero_retries() {
    let config = FallbackConfig::new(vec!["a".into()]).max_retries(0);
    assert_eq!(config.max_retries, 0);
}

#[test]
fn fallback_config_custom_backoff() {
    let config = FallbackConfig::new(vec!["a".into()]).initial_backoff(Duration::from_secs(5));
    assert_eq!(config.initial_backoff, Duration::from_secs(5));
}

/// A chain must stop dialling a provider whose circuit is open, rather than
/// spending its retry budget on a target it already knows is dead. The proof is
/// that the dead upstream sees only the first call's attempts and no more.
#[tokio::test]
async fn a_chain_skips_a_provider_with_an_open_circuit() {
    use llmshim::breaker::{BreakerConfig, ProviderBreaker};
    use llmshim::providers::openai_compat::OpenAiCompatible;
    use std::time::Duration;

    // Default chain retries (2, so three attempts per model) with a breaker
    // that trips on the first failure: the dead upstream must see exactly one
    // chain attempt's worth of transport retries and nothing more. The shared
    // HTTP client applies its own reactive retries underneath, so derive the
    // expected request count rather than hard-coding it.
    let config = FallbackConfig::new(vec!["dead/m".into(), "alive/m".into()])
        .initial_backoff(Duration::from_millis(1));
    assert_eq!(config.max_retries, 2, "this test relies on chain retries");
    let transport_attempts = 1 + std::env::var("LLMSHIM_MAX_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(3);

    let mut dead = mockito::Server::new_async().await;
    let mut alive = mockito::Server::new_async().await;
    let down = dead
        .mock("POST", "/chat/completions")
        .with_status(503)
        .with_body("upstream down")
        .expect(transport_attempts)
        .create_async()
        .await;
    let up = alive
        .mock("POST", "/chat/completions")
        .with_body(
            serde_json::json!({
                "id": "r", "model": "good",
                "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                "usage": {}
            })
            .to_string(),
        )
        .expect(2)
        .create_async()
        .await;

    let router = llmshim::router::Router::new()
        .register(
            "dead",
            Box::new(OpenAiCompatible::new("dead", dead.url(), None)),
        )
        .register(
            "alive",
            Box::new(OpenAiCompatible::new("alive", alive.url(), None)),
        )
        .with_breaker(std::sync::Arc::new(ProviderBreaker::with_config(
            BreakerConfig {
                window: Duration::from_secs(60),
                trip_threshold: 1,
                cooldown: Duration::from_secs(300),
            },
        )));

    let request = serde_json::json!({
        "model": "dead/m",
        "messages": [{"role": "user", "content": "hi"}],
    });

    for _ in 0..2 {
        let result = llmshim::completion_with_fallback(&router, &request, &config, None).await;
        assert!(
            result.is_ok(),
            "the chain must still succeed via the healthy provider"
        );
    }

    // This expectation fails if the second call dialled the dead provider again.
    down.assert_async().await;
    up.assert_async().await;
}
