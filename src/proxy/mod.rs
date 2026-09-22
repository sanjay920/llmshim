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
    request.extensions_mut().insert(preparation_permit);
    next.run(request).await
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
    axum::Router::new()
        .route("/v1/chat", post(handlers::chat))
        .route("/v1/chat/completions", post(handlers::chat))
        .route("/v1/messages", post(handlers::chat))
        .route("/v1/chat/stream", post(handlers::chat_stream))
        .route("/v1/models", get(handlers::list_models))
        .route("/health", get(handlers::health))
        .layer(axum::middleware::from_fn(wire::translate))
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
