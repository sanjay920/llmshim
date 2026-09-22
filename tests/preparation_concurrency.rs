#![cfg(feature = "proxy")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use llmshim::provider::{Provider, ProviderRequest};
use llmshim::proxy::ratelimit::{Backpressure, InMemoryRateLimiter, RateLimitConfig};
use llmshim::proxy::{app_with_state, AppState};
use llmshim::router::Router;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tower::ServiceExt;

struct LatchBlockedProvider {
    preparation_starts: Arc<AtomicUsize>,
    preparation_started: Arc<Notify>,
    preparation_latch: Arc<Notify>,
}

impl Provider for LatchBlockedProvider {
    fn name(&self) -> &str {
        "blocked"
    }

    fn transform_request(
        &self,
        _model: &str,
        _request: &Value,
    ) -> llmshim::error::Result<ProviderRequest> {
        unreachable!("the test preparation latch is never released")
    }

    fn prepare_request<'a>(
        &'a self,
        _model: &'a str,
        _request: &'a Value,
    ) -> Pin<Box<dyn Future<Output = llmshim::error::Result<ProviderRequest>> + Send + 'a>> {
        Box::pin(async move {
            self.preparation_starts.fetch_add(1, Ordering::SeqCst);
            self.preparation_started.notify_one();
            self.preparation_latch.notified().await;
            unreachable!("the test preparation latch is never released")
        })
    }

    fn transform_response(&self, _model: &str, _response: Value) -> llmshim::error::Result<Value> {
        unreachable!("the test never receives a provider response")
    }

    fn transform_stream_chunk(
        &self,
        _model: &str,
        _chunk: &str,
    ) -> llmshim::error::Result<Option<String>> {
        unreachable!("the test never opens a provider stream")
    }
}

async fn post_json(
    application: axum::Router,
    path: &str,
    payload: Value,
) -> axum::http::Response<Body> {
    application
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn preparation_backpressure_bounds_concurrent_http_requests_before_network_send() {
    let preparation_starts = Arc::new(AtomicUsize::new(0));
    let preparation_started = Arc::new(Notify::new());
    let first_preparation_started = preparation_started.notified();
    let state = Arc::new(AppState {
        router: Router::new().register(
            "blocked",
            Box::new(LatchBlockedProvider {
                preparation_starts: preparation_starts.clone(),
                preparation_started: preparation_started.clone(),
                preparation_latch: Arc::new(Notify::new()),
            }),
        ),
        logger: None,
        limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
        backpressure: Backpressure::new(1, Duration::from_millis(30)),
    });
    let application = app_with_state(state);
    let canonical_request = || {
        serde_json::json!({
            "model": "blocked/model",
            "messages": [{"role": "user", "content": "hi"}]
        })
    };

    let first_request = tokio::spawn(post_json(
        application.clone(),
        "/v1/chat",
        canonical_request(),
    ));
    tokio::time::timeout(Duration::from_secs(1), first_preparation_started)
        .await
        .expect("first request should enter provider preparation");
    assert_eq!(preparation_starts.load(Ordering::SeqCst), 1);

    let canonical_stream =
        post_json(application.clone(), "/v1/chat/stream", canonical_request()).await;
    assert_eq!(canonical_stream.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(canonical_stream.headers().contains_key("retry-after"));

    let native_unary = post_json(
        application.clone(),
        "/v1/chat/completions",
        canonical_request(),
    )
    .await;
    assert_eq!(native_unary.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(native_unary.headers().contains_key("retry-after"));
    let native_unary_body: Value =
        serde_json::from_slice(&to_bytes(native_unary.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(native_unary_body["error"]["type"], "api_error");

    let native_stream = post_json(
        application.clone(),
        "/v1/messages",
        serde_json::json!({
            "model": "blocked/model",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
    )
    .await;
    assert_eq!(native_stream.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(native_stream.headers().contains_key("retry-after"));
    let native_stream_body: Value =
        serde_json::from_slice(&to_bytes(native_stream.into_body(), 10_000).await.unwrap())
            .unwrap();
    assert_eq!(native_stream_body["error"]["type"], "overloaded_error");
    assert_eq!(preparation_starts.load(Ordering::SeqCst), 1);
    first_request.abort();

    let released_response = tokio::time::timeout(
        Duration::from_secs(1),
        post_json(
            application,
            "/v1/chat",
            serde_json::json!({
                "model": "unknown/model",
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ),
    )
    .await
    .expect("cancellation should release ingress preparation capacity");
    assert_eq!(released_response.status(), StatusCode::BAD_REQUEST);
}
