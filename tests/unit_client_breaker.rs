//! Provider health is counted where the dispatch happens — in `ShimClient` —
//! so a caller that resolves its own provider and comes straight to the client
//! feeds the same breaker as the crate's top-level entry points, and one
//! caller-visible call is one observation whichever door it came in by.
use llmshim::breaker::{BreakerConfig, ProviderBreaker};
use llmshim::client::ShimClient;
use llmshim::providers::openai_compat::OpenAiCompatible;
use llmshim::router::Router;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Trips on the second eligible failure. A path that observed one call twice
/// would open the circuit on that single call; a path that never observed
/// would never open it at all.
fn breaker() -> Arc<ProviderBreaker> {
    Arc::new(ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 2,
        cooldown: Duration::from_secs(300),
    }))
}

/// A 503 is both health-counting and transport-retryable, so the client dials
/// it several times per call. `Retry-After: 0` keeps those retries instant.
async fn dead(server: &mut mockito::ServerGuard) -> mockito::Mock {
    server
        .mock("POST", "/chat/completions")
        .with_status(503)
        .with_header("retry-after", "0")
        .with_body("upstream down")
        .expect_at_least(1)
        .create_async()
        .await
}

fn ok_body() -> String {
    json!({
        "id": "r", "model": "m",
        "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {}
    })
    .to_string()
}

#[tokio::test]
async fn a_direct_client_counts_each_call_once_and_a_success_closes_the_circuit() {
    let mut server = mockito::Server::new_async().await;
    let down = dead(&mut server).await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let health = breaker();
    let client = ShimClient::new().with_breaker(health.clone());
    let request = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});

    assert!(client.completion(&provider, "m", &request).await.is_err());
    assert!(
        !health.should_skip("local"),
        "the client's own transport retries are one dispatch, not several"
    );
    assert!(client.completion(&provider, "m", &request).await.is_err());
    assert!(
        health.should_skip("local"),
        "the second failed call must be the one that opens the circuit"
    );

    down.remove_async().await;
    let up = server
        .mock("POST", "/chat/completions")
        .with_body(ok_body())
        .expect(1)
        .create_async()
        .await;
    // A direct call is observed but never gated: with no alternative target,
    // refusing would only turn an upstream failure into a local one.
    assert!(client.completion(&provider, "m", &request).await.is_ok());
    assert!(!health.should_skip("local"), "success closes it again");
    up.assert_async().await;
}

#[tokio::test]
async fn the_top_level_entry_points_count_through_the_client_and_not_again() {
    let mut server = mockito::Server::new_async().await;
    let _down = dead(&mut server).await;
    let health = breaker();
    let router = Router::new()
        .register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        )
        .with_breaker(health.clone());
    let request = json!({"model": "local/m", "messages": [{"role": "user", "content": "hi"}]});

    assert!(llmshim::completion(&router, &request).await.is_err());
    assert!(
        !health.should_skip("local"),
        "one top-level completion observed twice would already have tripped"
    );
    assert!(llmshim::completion(&router, &request).await.is_err());
    assert!(health.should_skip("local"));

    // Same contract for the streaming door: the verdict is whether it opened.
    let health = breaker();
    let router = Router::new()
        .register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        )
        .with_breaker(health.clone());
    let mut streaming = request.clone();
    streaming["stream"] = json!(true);
    assert!(llmshim::stream(&router, &streaming).await.is_err());
    assert!(
        !health.should_skip("local"),
        "one failed stream open is one observation"
    );
    assert!(llmshim::stream(&router, &streaming).await.is_err());
    assert!(health.should_skip("local"));
}
