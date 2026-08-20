//! Local HTTP gateway (feature `gateway`).
//!
//! Serves the same wire contract as `llmshim proxy`'s `POST /v1/chat`, but every
//! non-streaming request flows through the priority [`Scheduler`] instead of the
//! proxy's synchronous admission control: it's enqueued per-provider and
//! dispatched when the provider's rate-limit token bucket has capacity, ordered
//! by priority tier then FIFO. The tier comes from an **`x-llmshim-priority`**
//! request header (unsigned integer, default `0`) — clients that don't set it
//! land in the base tier, so it stays a drop-in for the proxy contract.
//!
//! Reuses the proxy's request/response converters, error mapping, rate limiter,
//! and token estimator. Streaming (`stream: true`) is **not yet queued** — the
//! gateway rejects it for now (deferred); use `llmshim proxy` for streaming.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde_json::Value;
use tower_http::cors::CorsLayer;

use crate::error::ShimError;
use crate::gateway::{
    Dispatch, DispatchError, GatewayConfig, GatewayError, GatewayRequest, Scheduler,
};
use crate::log::{Logger, RequestTimer};
use crate::models;
use crate::proxy::convert::{request_to_value, value_to_response};
use crate::proxy::error::ApiError;
use crate::proxy::ratelimit::{build_limiter, estimate_request_tokens, penalty_duration};
use crate::proxy::types::{ChatRequest, HealthResponse, ModelEntry, ModelsResponse};
use crate::router::Router;

/// The [`Dispatch`] that actually fires an upstream LLM call once the scheduler
/// admits a job — via the crate's normal `completion` path. Detects an upstream
/// 429 so the scheduler can penalize the provider's bucket before serving more.
pub struct RealDispatch {
    router: Arc<Router>,
    logger: Option<Logger>,
}

#[async_trait]
impl Dispatch for RealDispatch {
    async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
        match crate::completion_with_logger(self.router.as_ref(), &payload, self.logger.as_ref())
            .await
        {
            Ok(value) => Ok(value),
            Err(ShimError::ProviderError { status: 429, body }) => Err(DispatchError {
                message: body,
                retry_after: Some(penalty_duration()),
            }),
            Err(err) => Err(DispatchError::new(err.to_string())),
        }
    }
}

/// Shared state for the gateway HTTP handlers.
pub struct GatewayState {
    router: Arc<Router>,
    scheduler: Arc<Scheduler>,
    /// `Retry-After` suggested when a job's queue wait times out.
    overloaded_retry_after: Duration,
}

impl GatewayState {
    /// Build gateway state, reading the rate limiter from the environment (same
    /// knobs as the proxy) and using default scheduler tuning.
    pub fn from_env(router: Router, logger: Option<Logger>) -> Arc<Self> {
        let config = GatewayConfig::from_env();
        let router = Arc::new(router);
        let dispatch = Arc::new(RealDispatch {
            router: router.clone(),
            logger,
        });
        let scheduler = Scheduler::new(config.clone(), build_limiter(), dispatch);
        Arc::new(Self {
            router,
            scheduler,
            overloaded_retry_after: config.overloaded_retry_after,
        })
    }
}

/// Parse the `x-llmshim-priority` header into a tier (default `0`).
fn priority_of(headers: &HeaderMap) -> u8 {
    headers
        .get("x-llmshim-priority")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0)
}

/// POST /v1/chat — enqueue by priority, dispatch when the provider has capacity.
async fn chat(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    if req.stream {
        return Err(ApiError::from(ShimError::ProviderError {
            status: 400,
            body: "streaming through the gateway queue is not yet supported; \
                   use `llmshim proxy` for streaming"
                .to_string(),
        }));
    }

    // Resolve the provider up front — this both validates the model (→ 400 on an
    // unknown provider/model) and gives us the lane key.
    let provider_name = {
        let (provider, _model) = state.router.resolve(&req.model)?;
        provider.name().to_string()
    };

    let tier = priority_of(&headers);
    let permits = estimate_request_tokens(&req);
    let payload = request_to_value(&req);

    let timer = RequestTimer::start();
    let result = state
        .scheduler
        .submit(GatewayRequest {
            provider: provider_name.clone(),
            tier,
            permits,
            payload,
        })
        .await;

    match result {
        Ok(resp) => {
            let elapsed = timer.elapsed().as_millis() as u64;
            Ok(Json(value_to_response(&resp, &provider_name, elapsed)).into_response())
        }
        Err(GatewayError::Overloaded(retry_after)) => Err(ApiError::Overloaded(retry_after)),
        Err(GatewayError::Timeout) => Err(ApiError::Overloaded(state.overloaded_retry_after)),
        Err(GatewayError::Shutdown) => Err(ApiError::from(ShimError::ProviderError {
            status: 503,
            body: "gateway shutting down".to_string(),
        })),
        Err(GatewayError::Upstream(message)) => Err(ApiError::from(ShimError::ProviderError {
            status: 502,
            body: message,
        })),
    }
}

/// GET /v1/models — models filtered to configured providers.
async fn list_models(State(state): State<Arc<GatewayState>>) -> Json<ModelsResponse> {
    let provider_keys = state.router.provider_keys();
    let entries = models::available_models(&provider_keys)
        .into_iter()
        .map(|m| ModelEntry {
            id: m.id.to_string(),
            provider: m.provider.to_string(),
            name: m.name.to_string(),
        })
        .collect();
    Json(ModelsResponse { models: entries })
}

/// GET /health — health check with the configured provider list.
async fn health(State(state): State<Arc<GatewayState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        providers: state
            .router
            .provider_keys()
            .into_iter()
            .map(String::from)
            .collect(),
    })
}

/// Build the gateway axum application.
pub fn app(state: Arc<GatewayState>) -> axum::Router {
    axum::Router::new()
        .route("/v1/chat", post(chat))
        .route("/v1/models", get(list_models))
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}
