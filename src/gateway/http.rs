//! Local HTTP gateway (feature `gateway`).
//!
//! Serves the same wire contract as `llmshim proxy` (`POST /v1/chat`,
//! `POST /v1/chat/stream`), but every request — unary or streaming — flows
//! through the priority [`Scheduler`] instead of the proxy's synchronous
//! admission control: it's enqueued per-provider and dispatched when the
//! provider's rate-limit token bucket has capacity, ordered by priority tier
//! then FIFO (with anti-starvation aging). The tier comes from an
//! **`x-llmshim-priority`** request header (unsigned integer, default `0`) —
//! clients that don't set it land in the base tier, so it stays a drop-in for
//! the proxy contract.
//!
//! Reuses the proxy's request/response converters, error mapping, rate limiter,
//! and token estimator.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use futures::StreamExt;
use serde_json::Value;
use tower_http::cors::CorsLayer;

use tokio::sync::mpsc;

use crate::error::ShimError;
use crate::gateway::{
    ChunkStream, Dispatch, DispatchError, GatewayConfig, GatewayError, GatewayRequest, Scheduler,
    StreamChunk,
};
use crate::log::{Logger, RequestTimer};
use crate::models;
use crate::proxy::convert::{chunk_to_events, request_to_value, value_to_response};
use crate::proxy::error::ApiError;
use crate::proxy::ratelimit::{build_limiter, estimate_request_tokens, penalty_duration};
use crate::proxy::types::{ChatRequest, HealthResponse, ModelEntry, ModelsResponse, StreamEvent};
use crate::router::Router;

/// The [`Dispatch`] that actually fires upstream LLM calls once the scheduler
/// admits a job — via the crate's normal `completion` / `stream` paths. Detects
/// an upstream 429 so the scheduler can penalize the provider's bucket.
pub struct RealDispatch {
    router: Arc<Router>,
    logger: Option<Logger>,
}

impl RealDispatch {
    fn map_err(err: ShimError) -> DispatchError {
        match err {
            ShimError::ProviderError { status: 429, body } => DispatchError {
                message: body,
                retry_after: Some(penalty_duration()),
            },
            other => DispatchError::new(other.to_string()),
        }
    }
}

#[async_trait]
impl Dispatch for RealDispatch {
    async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
        crate::completion_with_logger(self.router.as_ref(), &payload, self.logger.as_ref())
            .await
            .map_err(Self::map_err)
    }

    async fn dispatch_stream(
        &self,
        _provider: &str,
        payload: Value,
    ) -> Result<ChunkStream, DispatchError> {
        let upstream = crate::stream(self.router.as_ref(), &payload)
            .await
            .map_err(Self::map_err)?;
        // Map raw ShimError chunks → GatewayError so the channel type is stable.
        let mapped = upstream.map(|item| item.map_err(|e| GatewayError::Upstream(e.to_string())));
        Ok(Box::pin(mapped))
    }
}

/// Where queued work is scheduled: a single-process in-memory scheduler, or a
/// Redis-backed distributed queue shared across a fleet.
enum Backend {
    Local(Arc<Scheduler>),
    #[cfg(feature = "redis-coordination")]
    Distributed(Arc<crate::gateway::distributed::DistributedGateway>),
}

/// Shared state for the gateway HTTP handlers.
pub struct GatewayState {
    router: Arc<Router>,
    backend: Backend,
    /// `Retry-After` suggested when a job's queue wait times out.
    overloaded_retry_after: Duration,
}

impl GatewayState {
    /// Build in-memory (single-instance) gateway state, reading the rate limiter
    /// and scheduler tuning from the environment (same rate-limit knobs as the
    /// proxy, plus `LLMSHIM_GATEWAY_*`).
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
            backend: Backend::Local(scheduler),
            overloaded_retry_after: config.overloaded_retry_after,
        })
    }

    /// Build distributed (fleet) gateway state backed by a shared Redis queue +
    /// response bus, and spawn this instance's worker loops. The rate limiter is
    /// the shared Redis limiter so the whole fleet coordinates.
    #[cfg(feature = "redis-coordination")]
    pub async fn distributed_from_env(
        router: Router,
        logger: Option<Logger>,
        redis_url: &str,
    ) -> Result<Arc<Self>, String> {
        let config = GatewayConfig::from_env();
        let router = Arc::new(router);
        let dispatch = Arc::new(RealDispatch {
            router: router.clone(),
            logger,
        });
        let gateway = crate::gateway::distributed::DistributedGateway::connect(
            redis_url,
            dispatch,
            build_limiter(),
            config.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        // This instance serves every configured provider's queue.
        let providers: Vec<String> = router
            .provider_keys()
            .into_iter()
            .map(String::from)
            .collect();
        gateway.spawn_workers(providers);
        Ok(Arc::new(Self {
            router,
            backend: Backend::Distributed(gateway),
            overloaded_retry_after: config.overloaded_retry_after,
        }))
    }

    async fn submit(&self, req: GatewayRequest) -> Result<Value, GatewayError> {
        match &self.backend {
            Backend::Local(scheduler) => scheduler.submit(req).await,
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.submit(req).await,
        }
    }

    async fn submit_stream(
        &self,
        req: GatewayRequest,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        match &self.backend {
            Backend::Local(scheduler) => scheduler.submit_stream(req).await,
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.submit_stream(req).await,
        }
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

/// Resolve the provider (validates the model) and build the queued request.
fn build_request(
    state: &GatewayState,
    headers: &HeaderMap,
    req: &ChatRequest,
) -> Result<(String, GatewayRequest), ApiError> {
    let provider_name = {
        let (provider, _model) = state.router.resolve(&req.model)?;
        provider.name().to_string()
    };
    let gw = GatewayRequest {
        provider: provider_name.clone(),
        tier: priority_of(headers),
        permits: estimate_request_tokens(req),
        payload: request_to_value(req),
    };
    Ok((provider_name, gw))
}

/// Map a non-success gateway outcome to an HTTP error response.
fn gateway_err_to_api(state: &GatewayState, err: GatewayError) -> ApiError {
    match err {
        GatewayError::Overloaded(retry_after) => ApiError::Overloaded(retry_after),
        GatewayError::Timeout => ApiError::Overloaded(state.overloaded_retry_after),
        GatewayError::Shutdown => ApiError::from(ShimError::ProviderError {
            status: 503,
            body: "gateway shutting down".to_string(),
        }),
        GatewayError::Upstream(message) => ApiError::from(ShimError::ProviderError {
            status: 502,
            body: message,
        }),
    }
}

/// POST /v1/chat — enqueue by priority, dispatch when the provider has capacity.
/// Delegates to the streaming path when `stream: true`.
async fn chat(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    if req.stream {
        return Ok(chat_stream_inner(state, headers, req).await);
    }

    let (provider_name, gw) = build_request(&state, &headers, &req)?;
    let timer = RequestTimer::start();
    match state.submit(gw).await {
        Ok(resp) => {
            let elapsed = timer.elapsed().as_millis() as u64;
            Ok(Json(value_to_response(&resp, &provider_name, elapsed)).into_response())
        }
        Err(err) => Err(gateway_err_to_api(&state, err)),
    }
}

/// POST /v1/chat/stream — always SSE, queued by priority.
async fn chat_stream(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    chat_stream_inner(state, headers, req).await
}

async fn chat_stream_inner(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    req: ChatRequest,
) -> Response {
    let gw = match build_request(&state, &headers, &req) {
        Ok((_, gw)) => gw,
        Err(e) => return e.into_response(),
    };

    // Admission (queue + rate) happens up front so a rejection is a proper
    // 429/503 before the SSE response begins, not an SSE error event.
    let mut rx = match state.submit_stream(gw).await {
        Ok(rx) => rx,
        Err(err) => return gateway_err_to_api(&state, err).into_response(),
    };

    let event_stream = async_stream::stream! {
        while let Some(item) = rx.recv().await {
            match item {
                Ok(chunk) => {
                    for event in chunk_to_events(&chunk) {
                        let event_type = stream_event_type(&event);
                        if let Ok(data) = serde_json::to_string(&event) {
                            yield Ok(Event::default().event(event_type).data(data));
                        }
                    }
                }
                Err(e) => {
                    let error_event = StreamEvent::Error { message: e.to_string() };
                    if let Ok(data) = serde_json::to_string(&error_event) {
                        yield Ok(Event::default().event("error").data(data));
                    }
                    break;
                }
            }
        }
    };

    // Pin the item type (all yields are `Ok`, so `E` is otherwise ambiguous).
    fn pin_item<S: futures::Stream<Item = Result<Event, Infallible>>>(s: S) -> S {
        s
    }
    Sse::new(pin_item(event_stream)).into_response()
}

/// SSE `event:` name for a typed [`StreamEvent`].
fn stream_event_type(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Content { .. } => "content",
        StreamEvent::Reasoning { .. } => "reasoning",
        StreamEvent::ToolCall { .. } => "tool_call",
        StreamEvent::Usage(_) => "usage",
        StreamEvent::Done { .. } => "done",
        StreamEvent::Error { .. } => "error",
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
        .route("/v1/chat/stream", post(chat_stream))
        .route("/v1/models", get(list_models))
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}
