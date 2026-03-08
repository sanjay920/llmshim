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
