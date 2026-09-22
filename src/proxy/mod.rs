mod attempt;
pub(crate) mod convert;
pub(crate) mod error;
mod handlers;
pub mod health;
pub(crate) mod origin;
pub mod ratelimit;
pub mod types;
pub mod wire;

use crate::log::Logger;
use crate::router::Router;
use axum::routing::{get, post};
use origin::OriginPolicy;
use ratelimit::{build_limiter, Backpressure, RateLimiter};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

#[derive(Clone)]
pub(crate) struct LogicalPreparationPermit {
    _permit: Arc<OwnedSemaphorePermit>,
}

impl LogicalPreparationPermit {
    async fn acquire(backpressure: &Backpressure) -> Result<Self, ()> {
        backpressure.acquire_preparation().await.map(|permit| Self {
            _permit: Arc::new(permit),
        })
    }
}

async fn admit_preparation(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let inference_path = request.uri().path();
    let requires_preparation = request.method() == axum::http::Method::POST
        && matches!(
            inference_path,
            "/v1/chat" | "/v1/chat/stream" | "/v1/chat/completions" | "/v1/messages"
        );
    if !requires_preparation {
        return next.run(request).await;
    }

    let preparation_permit = match LogicalPreparationPermit::acquire(&state.backpressure).await {
        Ok(permit) => permit,
        Err(()) => {
            return preparation_overload_response(
                inference_path,
                state.backpressure.queue_timeout(),
            )
        }
    };
    request.extensions_mut().insert(preparation_permit.clone());
    let response = next.run(request).await;
    drop(preparation_permit);
    response
}

fn preparation_overload_response(
    path: &str,
    queue_timeout: std::time::Duration,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let mut response = match path {
        "/v1/chat/completions" => wire::fail(
            wire::Wire::Chat,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Proxy is at capacity; retry after the suggested delay",
        ),
        "/v1/messages" => wire::fail(
            wire::Wire::Messages,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Proxy is at capacity; retry after the suggested delay",
        ),
        _ => error::ApiError::Overloaded(queue_timeout).into_response(),
    };
    let retry_after_seconds =
        queue_timeout.as_secs() + u64::from(queue_timeout.subsec_millis() > 0);
    if let Ok(retry_after) =
        axum::http::HeaderValue::from_str(&retry_after_seconds.max(1).to_string())
    {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, retry_after);
    }
    response
}

/// Shared state for all proxy handlers.
pub struct AppState {
    pub router: Router,
    pub logger: Option<Logger>,
    /// Proactive rate-limit coordinator (in-memory by default, Redis opt-in).
    pub limiter: Arc<dyn RateLimiter>,
    /// Per-instance concurrency cap + bounded queue (load shedding).
    pub backpressure: Backpressure,
}

impl AppState {
    /// Construct proxy state, reading rate-limit + backpressure config from the
    /// environment (all optional with safe defaults).
    pub fn from_env(router: Router, logger: Option<Logger>) -> Self {
        // Provider health is coordinated the same way rate limits are, so a
        // fleet agrees on which providers are down.
        let router = router.with_breaker(health::build_breaker());
        Self {
            router,
            logger,
            limiter: build_limiter(),
            backpressure: Backpressure::from_env(),
        }
    }
}

/// Build the axum application with all routes.
pub fn app(router: Router, logger: Option<Logger>) -> axum::Router {
    app_with_state(Arc::new(AppState::from_env(router, logger)))
}

/// Build the axum application from a pre-constructed state. Lets tests inject a
/// custom limiter / backpressure without touching the environment.
pub fn app_with_state(state: Arc<AppState>) -> axum::Router {
    app_with_origin_policy(state, OriginPolicy::from_env())
}

pub(crate) fn app_with_origin_policy(
    state: Arc<AppState>,
    origin_policy: OriginPolicy,
) -> axum::Router {
    let default_receipt_store = wire::DefaultReceiptStore::from_env();
    axum::Router::new()
        .route("/v1/chat", post(handlers::chat))
        .route("/v1/chat/completions", post(handlers::chat))
        .route("/v1/messages", post(handlers::chat))
        .route("/v1/chat/stream", post(handlers::chat_stream))
        .route("/v1/models", get(handlers::list_models))
        .route("/health", get(handlers::health))
        .layer(axum::middleware::from_fn(wire::translate))
        .layer(axum::middleware::from_fn_with_state(
            default_receipt_store,
            wire::install_default_receipt_store,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admit_preparation,
        ))
        .layer(origin_policy.cors_layer())
        .layer(axum::middleware::from_fn(move |request, next| {
            origin::admit_browser_origin(origin_policy.clone(), request, next)
        }))
        .with_state(state)
}

#[cfg(test)]
mod preparation_lifetime_tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::{Request, StatusCode};
    use axum::response::sse::{Event, Sse};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::convert::Infallible;
    use std::time::Duration;
    use tokio::sync::{Notify, Semaphore};
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        Arc::new(AppState {
            router: Router::new(),
            logger: None,
            limiter: Arc::new(ratelimit::InMemoryRateLimiter::new(
                ratelimit::RateLimitConfig::default(),
            )),
            backpressure: Backpressure::new(1, Duration::from_millis(20)),
        })
    }

    fn native_request(path: &str, stream: bool) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "local/test",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": stream
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn native_unary_response_translation_retains_outer_preparation_capacity() {
        let response_buffering_started = Arc::new(Notify::new());
        let first_response_buffering_started = response_buffering_started.notified();
        let release_response = Arc::new(Semaphore::new(0));
        let handler = {
            let response_buffering_started = response_buffering_started.clone();
            let release_response = release_response.clone();
            move |Extension(_handler_permit): Extension<LogicalPreparationPermit>| {
                let response_buffering_started = response_buffering_started.clone();
                let release_response = release_response.clone();
                async move {
                    let body = async_stream::stream! {
                        response_buffering_started.notify_one();
                        release_response
                            .acquire()
                            .await
                            .expect("test release semaphore remains open")
                            .forget();
                        yield Ok::<_, Infallible>(serde_json::json!({
                            "id": "response",
                            "model": "local/test",
                            "message": {"role": "assistant", "content": "ok"},
                            "usage": {},
                            "finish_reason": "stop"
                        }).to_string());
                    };
                    (
                        [((axum::http::header::CONTENT_TYPE), "application/json")],
                        Body::from_stream(body),
                    )
                }
            }
        };
        let state = state();
        let application = axum::Router::new()
            .route("/v1/chat/completions", post(handler))
            .layer(axum::middleware::from_fn(wire::translate))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                admit_preparation,
            ))
            .with_state(state);

        let first_request = tokio::spawn(
            application
                .clone()
                .oneshot(native_request("/v1/chat/completions", false)),
        );
        tokio::time::timeout(Duration::from_secs(1), first_response_buffering_started)
            .await
            .expect("native unary translation should begin buffering");
        let saturated = application
            .clone()
            .oneshot(native_request("/v1/chat/completions", false))
            .await
            .unwrap();
        assert_eq!(saturated.status(), StatusCode::SERVICE_UNAVAILABLE);
        release_response.add_permits(1);
        let completed = first_request.await.unwrap().unwrap();
        assert_eq!(completed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn never_polled_native_sse_body_retains_handler_preparation_capacity() {
        let handler = |Extension(handler_permit): Extension<LogicalPreparationPermit>| async move {
            let stream = async_stream::stream! {
                let _handler_permit = handler_permit;
                std::future::pending::<()>().await;
                yield Ok::<Event, Infallible>(Event::default());
            };
            Sse::new(stream).into_response()
        };
        let state = state();
        let application = axum::Router::new()
            .route("/v1/messages", post(handler))
            .layer(axum::middleware::from_fn(wire::translate))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                admit_preparation,
            ))
            .with_state(state);

        let never_polled = application
            .clone()
            .oneshot(native_request("/v1/messages", true))
            .await
            .unwrap();
        assert_eq!(never_polled.status(), StatusCode::OK);
        let saturated = application
            .clone()
            .oneshot(native_request("/v1/messages", true))
            .await
            .unwrap();
        assert_eq!(saturated.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(never_polled);

        let admitted = tokio::time::timeout(
            Duration::from_secs(1),
            application.oneshot(native_request("/v1/messages", true)),
        )
        .await
        .expect("dropping an unpolled SSE body should release preparation capacity")
        .unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
    }
}
