mod convert;
mod error;
mod handlers;
pub mod ratelimit;
pub mod types;

use crate::log::Logger;
use crate::router::Router;
use axum::routing::{get, post};
use ratelimit::{build_limiter, Backpressure, RateLimiter};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

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
    axum::Router::new()
        .route("/v1/chat", post(handlers::chat))
        .route("/v1/chat/stream", post(handlers::chat_stream))
        .route("/v1/models", get(handlers::list_models))
        .route("/health", get(handlers::health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}
