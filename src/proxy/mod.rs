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
        .layer(origin_policy.cors_layer())
        .layer(axum::middleware::from_fn(move |request, next| {
            origin::admit_browser_origin(origin_policy.clone(), request, next)
        }))
        .with_state(state)
}
