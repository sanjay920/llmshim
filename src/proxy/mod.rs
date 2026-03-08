mod convert;
mod error;
mod handlers;
pub mod types;

use crate::log::Logger;
use crate::router::Router;
use axum::routing::{get, post};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

pub struct AppState {
    pub router: Router,
    pub logger: Option<Logger>,
}

/// Build the axum application with all routes.
pub fn app(router: Router, logger: Option<Logger>) -> axum::Router {
    let state = Arc::new(AppState { router, logger });

    axum::Router::new()
        .route("/v1/chat", post(handlers::chat))
        .route("/v1/chat/stream", post(handlers::chat_stream))
        .route("/v1/models", get(handlers::list_models))
        .route("/health", get(handlers::health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}
