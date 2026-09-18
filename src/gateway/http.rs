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
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use tower_http::cors::CorsLayer;

use tokio::sync::mpsc;

use crate::error::ShimError;
use crate::gateway::{
    ChunkStream, Dispatch, DispatchError, GatewayConfig, GatewayError, GatewayRequest, Scheduler,
    StreamChunk,
};
use crate::log::{Logger, RequestTimer};
use crate::proxy::convert::{chunk_to_events, request_to_value, value_to_response};
use crate::proxy::error::ApiError;
use crate::proxy::ratelimit::{build_limiter, estimate_request_tokens, penalty_duration};
use crate::proxy::types::{ChatRequest, HealthResponse, ModelsResponse, StreamEvent};
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
    keystore: crate::gateway::auth::KeyStore,
    quota: crate::gateway::quota::TenantQuota,
    /// Per-identity USD cap, checked before dispatch and charged after.
    spend: crate::gateway::quota::SpendCap,
    idempotency: crate::gateway::idempotency::IdempotencyCache,
    #[cfg_attr(not(feature = "redis-coordination"), allow(dead_code))]
    idempotency_ttl_secs: u64,
    /// `Retry-After` suggested when a job's queue wait times out.
    overloaded_retry_after: Duration,
}

impl GatewayState {
    /// Build in-memory (single-instance) gateway state, reading the rate limiter
    /// and scheduler tuning from the environment (same rate-limit knobs as the
    /// proxy, plus `LLMSHIM_GATEWAY_*`).
    pub fn from_env(router: Router, logger: Option<Logger>) -> Arc<Self> {
        let config = GatewayConfig::from_env();
        // Same fleet-wide provider health the proxy uses.
        let router = Arc::new(router.with_breaker(crate::proxy::health::build_breaker()));
        let dispatch = Arc::new(RealDispatch {
            router: router.clone(),
            logger,
        });
        let scheduler = Scheduler::new(config.clone(), build_limiter(), dispatch);
        Arc::new(Self {
            router,
            backend: Backend::Local(scheduler),
            keystore: crate::gateway::auth::KeyStore::from_env(),
            quota: crate::gateway::quota::TenantQuota::new(),
            spend: crate::gateway::quota::SpendCap::in_memory(),
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idem_ttl_secs()),
            ),
            idempotency_ttl_secs: idem_ttl_secs(),
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
        // Same fleet-wide provider health the proxy uses.
        let router = Arc::new(router.with_breaker(crate::proxy::health::build_breaker()));
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
        // Spend is shared through the same Redis the queue uses, so a dollar
        // cap means the same thing on every replica.
        let spend = crate::gateway::quota::SpendCap::with_store(gateway.clone());
        Ok(Arc::new(Self {
            router,
            backend: Backend::Distributed(gateway),
            keystore: crate::gateway::auth::KeyStore::from_env(),
            quota: crate::gateway::quota::TenantQuota::new(),
            spend,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idem_ttl_secs()),
            ),
            idempotency_ttl_secs: idem_ttl_secs(),
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

    /// Refresh the queue-depth gauges just before a metrics scrape.
    async fn update_queue_depth_gauges(&self) {
        use crate::gateway::metrics;
        let depths = match &self.backend {
            Backend::Local(scheduler) => scheduler.lane_depths(),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => {
                let providers: Vec<String> = self
                    .router
                    .provider_keys()
                    .into_iter()
                    .map(String::from)
                    .collect();
                gateway.queue_depths(&providers).await
            }
        };
        for (provider, depth) in depths {
            metrics::gauge_set(
                metrics::QUEUE_DEPTH,
                &[("provider", &provider)],
                depth as i64,
            );
        }
    }

    /// Cached response for an `Idempotency-Key`, if any.
    async fn idem_lookup(&self, key: &str) -> Option<Value> {
        match &self.backend {
            Backend::Local(_) => self.idempotency.get(key),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.idem_get(key).await,
        }
    }

    /// Cache a completed response under an `Idempotency-Key`.
    async fn idem_store(&self, key: &str, value: &Value) {
        match &self.backend {
            Backend::Local(_) => self.idempotency.put(key, value.clone()),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => {
                gateway
                    .idem_put(key, value, self.idempotency_ttl_secs)
                    .await
            }
        }
    }

    /// Readiness: in-memory mode is always ready; distributed mode requires
    /// Redis to answer a PING.
    async fn ready(&self) -> bool {
        match &self.backend {
            Backend::Local(_) => true,
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.ping().await,
        }
    }

    /// Introspection snapshot for the admin stats endpoint.
    async fn stats(&self) -> Value {
        let providers: Vec<String> = self
            .router
            .provider_keys()
            .into_iter()
            .map(String::from)
            .collect();
        let (mode, depths) = match &self.backend {
            Backend::Local(scheduler) => ("in-memory", scheduler.lane_depths()),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => {
                ("distributed", gateway.queue_depths(&providers).await)
            }
        };
        let mut lanes = Vec::new();
        for (provider, depth) in depths {
            #[cfg_attr(not(feature = "redis-coordination"), allow(unused_mut))]
            let mut lane = json!({ "provider": provider, "queue_depth": depth });
            #[cfg(feature = "redis-coordination")]
            if let Backend::Distributed(gateway) = &self.backend {
                lane["dead_letter"] = json!(gateway.dead_letter_len(&provider).await);
            }
            lanes.push(lane);
        }
        json!({
            "mode": mode,
            "auth_enforced": self.keystore.is_enforced(),
            "providers": providers,
            "lanes": lanes,
        })
    }

    /// Enforce the caller's per-tenant quota (no-op in open/dev mode). Over
    /// quota → 429 + `Retry-After`.
    fn enforce_quota(
        &self,
        identity: &crate::gateway::auth::Identity,
        provider: &str,
        permits: u32,
    ) -> Result<(), ApiError> {
        if let Err(retry) = self.quota.check(
            &identity.tenant,
            provider,
            identity.rpm,
            identity.tpm,
            permits,
        ) {
            crate::gateway::metrics::incr(
                crate::gateway::metrics::REJECTED,
                &[("provider", provider), ("reason", "tenant_quota")],
            );
            return Err(ApiError::RateLimited(retry));
        }
        Ok(())
    }
}

impl GatewayState {
    /// Reject a caller that has already spent its window's budget. Dollars are
    /// fungible across providers, so the ledger is keyed by tenant alone.
    async fn enforce_budget(
        &self,
        identity: &crate::gateway::auth::Identity,
        provider: &str,
        model: &str,
    ) -> Result<(), ApiError> {
        use crate::gateway::quota::BudgetRefusal;
        match self.spend.check(identity, provider, model).await {
            Ok(()) => {
                // An operator who opted in still gets told, every time. An
                // accepted risk that stops being visible becomes an assumption.
                if crate::gateway::quota::SpendCap::is_unpriced_under_cap(identity, provider, model)
                {
                    crate::gateway::metrics::incr(
                        crate::gateway::metrics::UNPRICED_UNDER_CAP,
                        &[("provider", provider), ("model", model)],
                    );
                    eprintln!(
                        "gateway: tenant {} running {provider}/{model} unpriced under a spend \
                         cap; this spend is NOT charged against the budget \
                         (budget_allow_unpriced is set)",
                        identity.tenant
                    );
                }
                Ok(())
            }
            Err(BudgetRefusal::Exhausted(retry)) => {
                crate::gateway::metrics::incr(
                    crate::gateway::metrics::REJECTED,
                    &[("provider", provider), ("reason", "tenant_budget")],
                );
                Err(ApiError::RateLimited(retry))
            }
            Err(BudgetRefusal::Unpriceable) => {
                crate::gateway::metrics::incr(
                    crate::gateway::metrics::REJECTED,
                    &[
                        ("provider", provider),
                        ("reason", "unpriceable_under_budget"),
                    ],
                );
                // Not a 429: retrying never clears this. The catalog has no
                // price for the target, so the cap cannot be enforced against it.
                Err(ApiError::Shim(crate::error::ShimError::ProviderError {
                    status: 400,
                    body: format!(
                        "{{\"error\":{{\"message\":\"no catalog price for '{provider}/{model}', \
                         so it cannot be charged against this key's spend budget. Use a priced \
                         model, add a local price override, or set budget_allow_unpriced on the \
                         key to run it uncharged.\",\"type\":\"invalid_request_error\",\
                         \"param\":\"model\",\"code\":\"unpriceable_under_budget\"}}}}"
                    ),
                }))
            }
        }
    }
}

/// Idempotency cache TTL (seconds) from the environment (default 300).
fn idem_ttl_secs() -> u64 {
    std::env::var("LLMSHIM_GATEWAY_IDEMPOTENCY_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Authenticate, resolve the provider (validates the model), and build the
/// queued request. Returns the caller's [`Identity`] so the handler can apply
/// per-tenant quotas. Tier comes from the identity (the key in enforced mode,
/// the header in open mode) — never a client header when auth is enforced.
fn build_request(
    state: &GatewayState,
    headers: &HeaderMap,
    req: &ChatRequest,
) -> Result<(String, GatewayRequest, crate::gateway::auth::Identity), ApiError> {
    let identity = state.keystore.identify(headers).map_err(|_| {
        crate::gateway::metrics::incr(
            crate::gateway::metrics::REJECTED,
            &[("provider", "unknown"), ("reason", "unauthorized")],
        );
        ApiError::Unauthorized
    })?;
    crate::proxy::convert::validate_request(req)?;
    let provider_name = {
        let (provider, _model) = state.router.resolve(&req.model)?;
        provider.name().to_string()
    };
    let gw = GatewayRequest {
        provider: provider_name.clone(),
        tier: identity.tier,
        permits: estimate_request_tokens(req),
        payload: request_to_value(req),
    };
    Ok((provider_name, gw, identity))
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

    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (provider_name, gw, identity) = build_request(&state, &headers, &req)?;
    state.enforce_quota(&identity, &provider_name, gw.permits)?;
    let (_, budget_model) = state.router.resolve(&req.model)?;
    state
        .enforce_budget(&identity, &provider_name, &budget_model)
        .await?;

    // Retry-safety: a repeated Idempotency-Key returns the first result.
    if let Some(key) = &idem_key {
        if let Some(cached) = state.idem_lookup(key).await {
            return Ok((
                [("idempotency-replayed", "true")],
                Json(value_to_response(&cached, &provider_name, 0)),
            )
                .into_response());
        }
    }

    let timer = RequestTimer::start();
    match state.submit(gw).await {
        Ok(resp) => {
            if let Some(key) = &idem_key {
                state.idem_store(key, &resp).await;
            }
            state
                .spend
                .record(&identity, crate::cost::stamped(&resp["usage"]))
                .await;
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
    let (provider_name, gw, identity) = match build_request(&state, &headers, &req) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = state.enforce_quota(&identity, &provider_name, gw.permits) {
        return e.into_response();
    }
    let budget_model = match state.router.resolve(&req.model) {
        Ok((_, m)) => m,
        Err(e) => return ApiError::from(e).into_response(),
    };
    if let Err(e) = state
        .enforce_budget(&identity, &provider_name, &budget_model)
        .await
    {
        return e.into_response();
    }

    // Admission (queue + rate) happens up front so a rejection is a proper
    // 429/503 before the SSE response begins, not an SSE error event.
    let mut rx = match state.submit_stream(gw).await {
        Ok(rx) => rx,
        Err(err) => return gateway_err_to_api(&state, err).into_response(),
    };

    let ledger = state.clone();
    let event_stream = async_stream::stream! {
        while let Some(item) = rx.recv().await {
            match item {
                Ok(chunk) => {
                    for event in chunk_to_events(&chunk) {
                        // A stream's cost arrives with its terminal usage event.
                        if let crate::proxy::types::StreamEvent::Usage(usage) = &event {
                            ledger.spend.record(&identity, usage.cost_usd).await;
                        }
                        let event_type = stream_event_type(&event);
                        if let Ok(data) = serde_json::to_string(&event) {
                            yield Ok(Event::default().event(event_type).data(data));
                        }
                    }
                }
                Err(e) => {
                    let error_event = crate::proxy::error::stream_error(&e.to_string());
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
    Json(crate::proxy::convert::models_response(
        &state.router.provider_keys(),
    ))
}

/// GET /metrics — Prometheus text exposition of gateway metrics.
async fn metrics(State(state): State<Arc<GatewayState>>) -> Response {
    state.update_queue_depth_gauges().await;
    let body = crate::gateway::metrics::render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// GET /ready — readiness probe (503 when a dependency, e.g. Redis, is down).
async fn ready(State(state): State<Arc<GatewayState>>) -> Response {
    if state.ready().await {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response()
    }
}

/// GET /v1/gateway/stats — queue depths, mode, dead-letter counts.
async fn stats(State(state): State<Arc<GatewayState>>) -> Json<Value> {
    Json(state.stats().await)
}

/// Ensure every request/response carries an `x-request-id` for correlation
/// (honoring an incoming one, else minting a monotonic id).
async fn request_id(mut req: Request, next: Next) -> Response {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "gw-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            )
        });
    if let Ok(hv) = HeaderValue::from_str(&id) {
        req.headers_mut().insert("x-request-id", hv.clone());
        let mut resp = next.run(req).await;
        resp.headers_mut().insert("x-request-id", hv);
        return resp;
    }
    next.run(req).await
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
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(chat))
        .route("/v1/chat/stream", post(chat_stream))
        .route("/v1/models", get(list_models))
        .route("/v1/gateway/stats", get(stats))
        .route("/metrics", get(metrics))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            native_translate,
        ))
        .layer(axum::middleware::from_fn(request_id))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn native_translate(
    State(state): State<Arc<GatewayState>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let wire = match request.uri().path() {
        "/v1/messages" => Some(crate::proxy::wire::Wire::Messages),
        "/v1/chat/completions" => Some(crate::proxy::wire::Wire::Chat),
        _ => None,
    };
    if let Some(wire) = wire {
        crate::proxy::wire::normalize_auth(request.headers_mut());
        if state.keystore.identify(request.headers()).is_err() {
            return crate::proxy::wire::fail(
                wire,
                StatusCode::UNAUTHORIZED,
                "Missing or invalid API key",
            );
        }
    }
    crate::proxy::wire::translate(request, next).await
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        Extension,
    };
    use serde_json::json;
    use tower::ServiceExt;

    fn configured_state(base_url: &str) -> Arc<GatewayState> {
        let router = Arc::new(Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local", base_url, None,
            )),
        ));
        let config = GatewayConfig::default();
        let limiter = Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
            crate::proxy::ratelimit::RateLimitConfig::default(),
        ));
        let scheduler = Scheduler::new(
            config.clone(),
            limiter,
            Arc::new(RealDispatch {
                router: router.clone(),
                logger: None,
            }),
        );
        let keys = std::collections::HashMap::from([(
            "test-key".into(),
            crate::gateway::auth::Identity {
                tenant: "test-tenant".into(),
                tier: 1,
                rpm: None,
                tpm: None,
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            },
        )]);
        let state = Arc::new(GatewayState {
            router,
            backend: Backend::Local(scheduler),
            keystore: crate::gateway::auth::KeyStore::enforced(keys),
            quota: crate::gateway::quota::TenantQuota::new(),
            spend: crate::gateway::quota::SpendCap::in_memory(),
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            overloaded_retry_after: config.overloaded_retry_after,
        });
        state
    }

    #[tokio::test]
    async fn native_routes_preserve_auth_queue_and_protocol_scoped_idempotency() {
        let mut server = mockito::Server::new_async().await;
        let upstream=server.mock("POST","/chat/completions").with_body(json!({"id":"r","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{}}).to_string()).expect(2).create_async().await;
        let state = configured_state(&server.url());
        let dir = tempfile::tempdir().unwrap();
        for path in ["/v1/messages", "/v1/chat/completions"] {
            let app = app(state.clone()).layer(Extension(Arc::new(
                crate::proxy::wire::Receipts::new(dir.path().to_owned()),
            )));
            let rejected = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
            let error: Value =
                serde_json::from_slice(&to_bytes(rejected.into_body(), 10000).await.unwrap())
                    .unwrap();
            assert_eq!(error["error"]["type"], "authentication_error");
            let response=app.oneshot(Request::builder().method("POST").uri(path).header("content-type","application/json").header("x-api-key","test-key").header("x-llmshim-priority","255").header("idempotency-key","same-client-key").body(Body::from(json!({"model":"local/test","messages":[{"role":"user","content":"hi"}]}).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().contains_key("x-request-id"));
            let body = to_bytes(response.into_body(), 10000).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("hello"));
        }
        upstream.assert_async().await;
    }
    #[tokio::test]
    async fn all_gateway_surfaces_normalize_upstream_errors_and_reject_bad_history() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server.mock("POST", "/chat/completions").with_status(401)
            .with_body(json!({"error":{"type":"invalid_request_error","code":"invalid_api_key","message":"API key is invalid.","param":null}}).to_string())
            .expect(5).create_async().await;
        let state = configured_state(&server.url());
        let application = app(state);
        for (path, stream) in [
            ("/v1/chat", false),
            ("/v1/chat", true),
            ("/v1/chat/stream", true),
            ("/v1/messages", false),
            ("/v1/chat/completions", false),
        ] {
            let response = application.clone().oneshot(Request::builder().method("POST").uri(path)
                .header("content-type","application/json").header("authorization","Bearer test-key")
                .body(Body::from(json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"stream":stream}).to_string())).unwrap()).await.unwrap();
            assert!(response.status().is_server_error());
            let data = to_bytes(response.into_body(), 100000).await.unwrap();
            let body: Value = serde_json::from_slice(&data).unwrap();
            assert_eq!(body["error"]["message"], "API key is invalid.");
            if path == "/v1/messages" {
                assert_eq!(body["error"]["type"], "authentication_error");
            } else {
                assert_eq!(body["error"]["code"], "invalid_api_key");
            }
        }
        for path in ["/v1/chat", "/v1/chat/stream", "/v1/chat/completions"] {
            let response = application.clone().oneshot(Request::builder().method("POST").uri(path)
                .header("content-type","application/json").header("authorization","Bearer test-key")
                .body(Body::from(json!({"model":"local/test","messages":[{"role":"assistant","content":"answer","tool_calls":{}}]}).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        upstream.assert_async().await;
    }
}
