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

use crate::proxy::origin::OriginPolicy;
use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

use crate::error::ShimError;
use crate::gateway::{
    ChunkStream, Dispatch, DispatchError, GatewayConfig, GatewayError, GatewayRequest, Scheduler,
    StreamChunk,
};
use crate::log::{Logger, RequestTimer};
use crate::proxy::convert::{chunk_to_events, value_to_response};
use crate::proxy::error::ApiError;
use crate::proxy::ratelimit::{build_limiter, estimate_prepared_request_tokens, penalty_duration};
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
            ShimError::ProviderError {
                status: 429,
                retry_after,
                ..
            } => DispatchError {
                message: format!(
                    "llmshim-rate-limit:{}",
                    retry_after.unwrap_or_else(penalty_duration).as_millis()
                ),
                retry_after: Some(retry_after.unwrap_or_else(penalty_duration)),
            },
            ShimError::ProviderError {
                status: 503,
                body,
                retry_after,
            } if body == "attempt policy coordinator unavailable" => match retry_after {
                Some(wait) => DispatchError {
                    message: format!("llmshim-overload:{}", wait.as_millis()),
                    retry_after: Some(wait),
                },
                None => DispatchError::new("llmshim-coordinator-unavailable"),
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

    async fn dispatch_with_policy(
        &self,
        _provider: &str,
        payload: Value,
        policy_context: crate::policy::DispatchPolicyContext,
    ) -> Result<Value, DispatchError> {
        crate::completion_with_logger_and_policy(
            self.router.as_ref(),
            &payload,
            self.logger.as_ref(),
            &policy_context,
        )
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

    async fn dispatch_stream_with_policy(
        &self,
        _provider: &str,
        payload: Value,
        policy_context: crate::policy::DispatchPolicyContext,
    ) -> Result<ChunkStream, DispatchError> {
        let upstream = crate::stream_with_policy(self.router.as_ref(), &payload, &policy_context)
            .await
            .map_err(Self::map_err)?;
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
    attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
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
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::local(
            crate::proxy::ratelimit::RateLimitConfig::from_env(),
            config.max_concurrency_per_provider,
            config.max_wait,
        );
        Arc::new(Self {
            router,
            backend: Backend::Local(scheduler),
            keystore: crate::gateway::auth::KeyStore::from_env(),
            attempt_coordinator,
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
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::redis(
            redis_url,
            crate::proxy::ratelimit::RateLimitConfig::from_env(),
            config.max_concurrency_per_provider,
            config.max_wait,
        )
        .map_err(|error| error.to_string())?;
        let gateway = crate::gateway::distributed::DistributedGateway::connect_with_coordinator(
            redis_url,
            dispatch,
            build_limiter(),
            config.clone(),
            attempt_coordinator.clone(),
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
            attempt_coordinator,
            spend,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idem_ttl_secs()),
            ),
            idempotency_ttl_secs: idem_ttl_secs(),
            overloaded_retry_after: config.overloaded_retry_after,
        }))
    }

    async fn submit(
        &self,
        req: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
    ) -> Result<Value, GatewayError> {
        match &self.backend {
            Backend::Local(scheduler) => {
                scheduler
                    .submit_with_policy(req, self.attempt_coordinator.context(policy_scope))
                    .await
            }
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.submit_with_policy(req, policy_scope).await,
        }
    }

    async fn submit_stream(
        &self,
        req: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        match &self.backend {
            Backend::Local(scheduler) => {
                scheduler
                    .submit_stream_with_policy(req, self.attempt_coordinator.context(policy_scope))
                    .await
            }
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => {
                gateway.submit_stream_with_policy(req, policy_scope).await
            }
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

    /// Cached response for a credential-, route-, and request-bound key.
    async fn idem_lookup(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
    ) -> crate::gateway::idempotency::IdempotencyLookup {
        match &self.backend {
            Backend::Local(_) => self.idempotency.lookup(context),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.scoped_idem_lookup(context).await,
        }
    }

    /// Cache a completed response under its request-bound key.
    async fn idem_store(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
        value: &Value,
    ) {
        match &self.backend {
            Backend::Local(_) => self.idempotency.store(context, value.clone()),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => {
                gateway
                    .scoped_idem_store(context, value, self.idempotency_ttl_secs)
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
                    retry_after: None,
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
) -> Result<
    (
        String,
        String,
        GatewayRequest,
        crate::gateway::auth::IdentifiedCaller,
    ),
    ApiError,
> {
    let identified_caller = state.keystore.identify_caller(headers).map_err(|_| {
        crate::gateway::metrics::incr(
            crate::gateway::metrics::REJECTED,
            &[("provider", "unknown"), ("reason", "unauthorized")],
        );
        ApiError::Unauthorized
    })?;
    let prepared_request = crate::proxy::convert::prepare_request(&state.router, req)?;
    let provider_name = prepared_request.target.provider_name.clone();
    let budget_model = prepared_request.target.model.clone();
    let permits = estimate_prepared_request_tokens(&prepared_request);
    let gw = GatewayRequest {
        provider: provider_name.clone(),
        tier: identified_caller.identity.tier,
        permits,
        payload: prepared_request.payload,
    };
    Ok((provider_name, budget_model, gw, identified_caller))
}

fn idempotency_conflict() -> ApiError {
    ApiError::from(ShimError::ProviderError {
        status: StatusCode::CONFLICT.as_u16(),
        body: json!({
            "error": {
                "message": "Idempotency-Key was already used with a different request",
                "type": "invalid_request_error",
                "param": "Idempotency-Key",
                "code": "idempotency_key_reused"
            }
        })
        .to_string(),
        retry_after: None,
    })
}

/// Map a non-success gateway outcome to an HTTP error response.
fn gateway_err_to_api(state: &GatewayState, err: GatewayError) -> ApiError {
    match err {
        GatewayError::Overloaded(retry_after) => ApiError::Overloaded(retry_after),
        GatewayError::Timeout => ApiError::Overloaded(state.overloaded_retry_after),
        GatewayError::Shutdown => ApiError::from(ShimError::ProviderError {
            status: 503,
            body: "gateway shutting down".to_string(),
            retry_after: None,
        }),
        GatewayError::Upstream(message) if message.starts_with("llmshim-rate-limit:") => message
            .strip_prefix("llmshim-rate-limit:")
            .and_then(|milliseconds| milliseconds.parse::<u64>().ok())
            .map(Duration::from_millis)
            .map(ApiError::RateLimited)
            .unwrap_or_else(|| {
                ApiError::from(ShimError::ProviderError {
                    status: 502,
                    body: "invalid internal rate-limit response".into(),
                    retry_after: None,
                })
            }),
        GatewayError::Upstream(message) if message.starts_with("llmshim-overload:") => message
            .strip_prefix("llmshim-overload:")
            .and_then(|milliseconds| milliseconds.parse::<u64>().ok())
            .map(Duration::from_millis)
            .map(ApiError::Overloaded)
            .unwrap_or_else(|| {
                ApiError::from(ShimError::ProviderError {
                    status: 502,
                    body: "invalid internal overload response".into(),
                    retry_after: None,
                })
            }),
        GatewayError::Upstream(message) if message == "llmshim-coordinator-unavailable" => {
            ApiError::from(ShimError::ProviderError {
                status: 503,
                body: "attempt policy coordinator unavailable".into(),
                retry_after: None,
            })
        }
        GatewayError::Upstream(message) => ApiError::from(ShimError::ProviderError {
            status: 502,
            body: message,
            retry_after: None,
        }),
    }
}

/// POST /v1/chat — enqueue by priority, dispatch when the provider has capacity.
/// Delegates to the streaming path when `stream: true`.
async fn chat(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    uri: Uri,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    if req.stream {
        return Ok(chat_stream_inner(state, headers, req).await);
    }

    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (provider_name, budget_model, gw, identified_caller) =
        build_request(&state, &headers, &req)?;
    let identity = &identified_caller.identity;
    state
        .enforce_budget(identity, &provider_name, &budget_model)
        .await?;
    let policy_scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(identity);

    let idempotency_context = idem_key.as_ref().map(|client_key| {
        crate::gateway::idempotency::IdempotencyContext::new(
            &identity.tenant,
            &identified_caller.credential_scope,
            uri.path(),
            client_key,
            &gw.payload,
        )
    });

    // Retry-safety: a repeated Idempotency-Key returns the first result.
    if let Some(context) = &idempotency_context {
        match state.idem_lookup(context).await {
            crate::gateway::idempotency::IdempotencyLookup::Replay(cached_response) => {
                return Ok((
                    [("idempotency-replayed", "true")],
                    Json(value_to_response(&cached_response, &provider_name, 0)),
                )
                    .into_response());
            }
            crate::gateway::idempotency::IdempotencyLookup::Conflict => {
                return Err(idempotency_conflict());
            }
            crate::gateway::idempotency::IdempotencyLookup::Miss => {}
        }
    }

    let timer = RequestTimer::start();
    match state.submit(gw, policy_scope).await {
        Ok(resp) => {
            if let Some(context) = &idempotency_context {
                state.idem_store(context, &resp).await;
            }
            state
                .spend
                .record(identity, crate::cost::stamped(&resp["usage"]))
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
    let (provider_name, budget_model, gw, identified_caller) =
        match build_request(&state, &headers, &req) {
            Ok(t) => t,
            Err(e) => return e.into_response(),
        };
    let identity = identified_caller.identity;
    if let Err(e) = state
        .enforce_budget(&identity, &provider_name, &budget_model)
        .await
    {
        return e.into_response();
    }
    let policy_scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(&identity);

    // Admission (queue + rate) happens up front so a rejection is a proper
    // 429/503 before the SSE response begins, not an SSE error event.
    let mut rx = match state.submit_stream(gw, policy_scope).await {
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
    app_with_origin_policy(state, OriginPolicy::from_env())
}

pub(crate) fn app_with_origin_policy(
    state: Arc<GatewayState>,
    origin_policy: OriginPolicy,
) -> axum::Router {
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
        .layer(origin_policy.cors_layer())
        .layer(axum::middleware::from_fn(move |request, next| {
            crate::proxy::origin::admit_browser_origin(origin_policy.clone(), request, next)
        }))
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
    use mockito::Matcher;
    use serde_json::json;
    use tower::ServiceExt;

    fn configured_state(base_url: &str) -> Arc<GatewayState> {
        configured_state_with_limits(base_url, None, None)
    }

    fn configured_state_with_limits(
        base_url: &str,
        requests_per_minute: Option<u32>,
        tokens_per_minute: Option<u32>,
    ) -> Arc<GatewayState> {
        let router = Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local", base_url, None,
            )),
        );
        configured_state_with_router_and_limits(router, requests_per_minute, tokens_per_minute)
    }

    fn configured_state_for_provider(base_url: &str, provider_name: &str) -> Arc<GatewayState> {
        let router = Router::new().register(
            provider_name,
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                provider_name,
                base_url,
                None,
            )),
        );
        configured_state_with_router(router)
    }

    fn configured_state_with_router(router: Router) -> Arc<GatewayState> {
        configured_state_with_router_and_limits(router, None, None)
    }

    fn configured_state_with_router_and_limits(
        router: Router,
        requests_per_minute: Option<u32>,
        tokens_per_minute: Option<u32>,
    ) -> Arc<GatewayState> {
        let router = Arc::new(router);
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
        let identity = crate::gateway::auth::Identity {
            tenant: "test-tenant".into(),
            tier: 1,
            rpm: requests_per_minute,
            tpm: tokens_per_minute,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
        };
        let keys = std::collections::HashMap::from([
            ("test-key".into(), identity.clone()),
            ("second-key".into(), identity),
        ]);
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::local(
            crate::proxy::ratelimit::RateLimitConfig::default(),
            config.max_concurrency_per_provider,
            config.max_wait,
        );
        Arc::new(GatewayState {
            router,
            backend: Backend::Local(scheduler),
            keystore: crate::gateway::auth::KeyStore::enforced(keys),
            attempt_coordinator,
            spend: crate::gateway::quota::SpendCap::in_memory(),
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            overloaded_retry_after: config.overloaded_retry_after,
        })
    }

    fn configured_state_with_attempt_coordinator(
        base_url: &str,
        attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
    ) -> Arc<GatewayState> {
        let router = Arc::new(Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local", base_url, None,
            )),
        ));
        let config = GatewayConfig::default();
        let scheduler = Scheduler::new(
            config.clone(),
            Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
                crate::proxy::ratelimit::RateLimitConfig::default(),
            )),
            Arc::new(RealDispatch {
                router: router.clone(),
                logger: None,
            }),
        );
        let identity = crate::gateway::auth::Identity {
            tenant: "test-tenant".into(),
            tier: 1,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
        };
        Arc::new(GatewayState {
            router,
            backend: Backend::Local(scheduler),
            keystore: crate::gateway::auth::KeyStore::enforced(std::collections::HashMap::from([
                ("test-key".into(), identity),
            ])),
            attempt_coordinator,
            spend: crate::gateway::quota::SpendCap::in_memory(),
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            overloaded_retry_after: config.overloaded_retry_after,
        })
    }

    fn refusal_request(path: &str, stream: bool) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(
                json!({
                    "model": "local/test",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": stream
                })
                .to_string(),
            ))
            .unwrap()
    }

    async fn assert_gateway_refusal_surfaces(state: Arc<GatewayState>, expects_retry_after: bool) {
        for (path, stream, expected_native_type) in [
            ("/v1/chat", false, None),
            ("/v1/chat/stream", true, None),
            ("/v1/chat/completions", false, Some("api_error")),
            ("/v1/chat/completions", true, Some("api_error")),
            ("/v1/messages", false, Some("overloaded_error")),
            ("/v1/messages", true, Some("overloaded_error")),
        ] {
            let response = app(state.clone())
                .oneshot(refusal_request(path, stream))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                response
                    .headers()
                    .contains_key(axum::http::header::RETRY_AFTER),
                expects_retry_after,
                "unexpected Retry-After behavior for {path}, stream={stream}"
            );
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap())
                    .unwrap();
            if let Some(expected_native_type) = expected_native_type {
                assert_eq!(body["error"]["type"], expected_native_type);
            }
        }
    }

    fn canonical_request(api_key: &str, idempotency_key: &str, prompt: &str) -> Request<Body> {
        canonical_request_for_model(api_key, idempotency_key, prompt, "local/test")
    }

    fn canonical_request_for_model(
        api_key: &str,
        idempotency_key: &str,
        prompt: &str,
        model: &str,
    ) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {api_key}"))
            .header("idempotency-key", idempotency_key)
            .body(Body::from(
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": prompt}]
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[cfg(feature = "redis-coordination")]
    struct CountingDispatch {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[cfg(feature = "redis-coordination")]
    #[async_trait::async_trait]
    impl Dispatch for CountingDispatch {
        async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
            let call_number = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            Ok(json!({
                "id": format!("response-{call_number}"),
                "model": payload["model"],
                "choices": [{
                    "message": {"role": "assistant", "content": format!("private-{call_number}")},
                    "finish_reason": "stop"
                }],
                "usage": {}
            }))
        }
    }

    #[cfg(feature = "redis-coordination")]
    fn distributed_state_for_tenant(
        router: Arc<Router>,
        gateway: Arc<crate::gateway::distributed::DistributedGateway>,
        config: &GatewayConfig,
        attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
        tenant: &str,
    ) -> Arc<GatewayState> {
        let identity = crate::gateway::auth::Identity {
            tenant: tenant.to_string(),
            tier: 1,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
        };
        Arc::new(GatewayState {
            router,
            backend: Backend::Distributed(gateway.clone()),
            keystore: crate::gateway::auth::KeyStore::enforced(std::collections::HashMap::from([
                ("reassigned-key".into(), identity),
            ])),
            attempt_coordinator,
            spend: crate::gateway::quota::SpendCap::with_store(gateway),
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            overloaded_retry_after: config.overloaded_retry_after,
        })
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_http_cache_does_not_cross_a_bearer_tenant_reassignment() {
        let Some(redis_url) = std::env::var("LLMSHIM_REDIS_URL").ok() else {
            return;
        };
        let provider_name = format!("itest-reassigned-{}", uuid::Uuid::new_v4().simple());
        let router = Arc::new(Router::new().register(
            &provider_name,
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                provider_name.clone(),
                "http://127.0.0.1:1",
                None,
            )),
        ));
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = GatewayConfig::default();
        let limiter = Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
            crate::proxy::ratelimit::RateLimitConfig::default(),
        ));
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::redis(
            &redis_url,
            crate::proxy::ratelimit::RateLimitConfig::default(),
            config.max_concurrency_per_provider,
            config.max_wait,
        )
        .unwrap();
        let gateway = crate::gateway::distributed::DistributedGateway::connect_with_coordinator(
            &redis_url,
            Arc::new(CountingDispatch {
                calls: call_count.clone(),
            }),
            limiter,
            config.clone(),
            attempt_coordinator.clone(),
        )
        .await
        .unwrap();
        gateway.spawn_workers(vec![provider_name.clone()]);

        let tenant_a_application = app(distributed_state_for_tenant(
            router.clone(),
            gateway.clone(),
            &config,
            attempt_coordinator.clone(),
            "tenant-a",
        ));
        let tenant_b_application = app(distributed_state_for_tenant(
            router,
            gateway,
            &config,
            attempt_coordinator,
            "tenant-b",
        ));
        let model = format!("{provider_name}/test");
        let idempotency_key = format!("reassignment-{}", uuid::Uuid::new_v4());

        let tenant_a_response = tenant_a_application
            .oneshot(canonical_request_for_model(
                "reassigned-key",
                &idempotency_key,
                "same-request",
                &model,
            ))
            .await
            .unwrap();
        let tenant_a_body: Value = serde_json::from_slice(
            &to_bytes(tenant_a_response.into_body(), 10000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tenant_a_body["message"]["content"], "private-1");

        let tenant_b_response = tenant_b_application
            .clone()
            .oneshot(canonical_request_for_model(
                "reassigned-key",
                &idempotency_key,
                "same-request",
                &model,
            ))
            .await
            .unwrap();
        assert!(!tenant_b_response
            .headers()
            .contains_key("idempotency-replayed"));
        let tenant_b_body: Value = serde_json::from_slice(
            &to_bytes(tenant_b_response.into_body(), 10000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tenant_b_body["message"]["content"], "private-2");

        let tenant_b_replay = tenant_b_application
            .oneshot(canonical_request_for_model(
                "reassigned-key",
                &idempotency_key,
                "same-request",
                &model,
            ))
            .await
            .unwrap();
        assert_eq!(tenant_b_replay.headers()["idempotency-replayed"], "true");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn canonical_idempotency_is_bound_to_credential_and_request() {
        let mut server = mockito::Server::new_async().await;
        let tenant_a_upstream = server
            .mock("POST", "/chat/completions")
            .match_body(Matcher::PartialJson(json!({
                "messages": [{"role": "user", "content": "tenant-a"}]
            })))
            .with_body(
                json!({"id":"a","choices":[{"message":{"role":"assistant","content":"private-a"},"finish_reason":"stop"}],"usage":{}})
                    .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let tenant_b_upstream = server
            .mock("POST", "/chat/completions")
            .match_body(Matcher::PartialJson(json!({
                "messages": [{"role": "user", "content": "tenant-b"}]
            })))
            .with_body(
                json!({"id":"b","choices":[{"message":{"role":"assistant","content":"private-b"},"finish_reason":"stop"}],"usage":{}})
                    .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let application = app(configured_state(&server.url()));

        let first_response = application
            .clone()
            .oneshot(canonical_request("test-key", "shared-key", "tenant-a"))
            .await
            .unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);
        let first_body: Value =
            serde_json::from_slice(&to_bytes(first_response.into_body(), 10000).await.unwrap())
                .unwrap();
        assert_eq!(first_body["message"]["content"], "private-a");

        let other_credential_response = application
            .clone()
            .oneshot(canonical_request("second-key", "shared-key", "tenant-b"))
            .await
            .unwrap();
        assert_eq!(other_credential_response.status(), StatusCode::OK);
        assert!(!other_credential_response
            .headers()
            .contains_key("idempotency-replayed"));
        let other_credential_body: Value = serde_json::from_slice(
            &to_bytes(other_credential_response.into_body(), 10000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(other_credential_body["message"]["content"], "private-b");

        let replayed_response = application
            .clone()
            .oneshot(canonical_request("test-key", "shared-key", "tenant-a"))
            .await
            .unwrap();
        assert_eq!(replayed_response.status(), StatusCode::OK);
        assert_eq!(replayed_response.headers()["idempotency-replayed"], "true");

        let conflicting_response = application
            .oneshot(canonical_request("test-key", "shared-key", "changed"))
            .await
            .unwrap();
        assert_eq!(conflicting_response.status(), StatusCode::CONFLICT);
        let conflict_body: Value = serde_json::from_slice(
            &to_bytes(conflicting_response.into_body(), 10000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(conflict_body["error"]["code"], "idempotency_key_reused");

        tenant_a_upstream.assert_async().await;
        tenant_b_upstream.assert_async().await;
    }

    #[tokio::test]
    async fn zero_tenant_quota_rejects_before_upstream_dispatch() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "unexpected",
                    "choices": [{"message": {"role": "assistant", "content": "unexpected"}, "finish_reason": "stop"}],
                    "usage": {}
                })
                .to_string(),
            )
            .expect(0)
            .create_async()
            .await;
        let state = configured_state_with_limits(&server.url(), Some(0), None);
        let response = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(
                        json!({
                            "model": "local/test",
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "60");
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn coordinator_unavailable_stays_503_on_unary_native_and_sse_open() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let coordinator = crate::gateway::attempt::AttemptCoordinator::unavailable_for_test(
            8,
            Duration::from_millis(25),
        );
        assert_gateway_refusal_surfaces(
            configured_state_with_attempt_coordinator(&server.url(), coordinator),
            false,
        )
        .await;
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn attempt_concurrency_exhaustion_is_503_overload_on_every_surface() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let coordinator = crate::gateway::attempt::AttemptCoordinator::local(
            crate::proxy::ratelimit::RateLimitConfig::default(),
            1,
            Duration::from_millis(25),
        );
        let _held = coordinator.hold_provider_for_test("local").await;
        assert_gateway_refusal_surfaces(
            configured_state_with_attempt_coordinator(&server.url(), coordinator),
            true,
        )
        .await;
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn tenant_rpm_one_allows_exactly_one_real_send() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .with_body("retryable")
            .expect(1)
            .create_async()
            .await;
        let state = configured_state_with_limits(&server.url(), Some(1), None);
        let response = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(
                        json!({
                            "model": "local/test",
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn forged_policy_fields_do_not_change_the_authenticated_tenant_scope() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "response",
                    "choices": [{
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {}
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let state = configured_state_with_limits(&server.url(), Some(1), None);
        for (index, forged_tenant) in ["forged-a", "forged-b"].into_iter().enumerate() {
            let response = app(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/chat")
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(
                            json!({
                                "model": "local/test",
                                "messages": [{"role": "user", "content": "hi"}],
                                "provider_config": {
                                    "tenant": forged_tenant,
                                    "identity": forged_tenant,
                                    "policy_id": forged_tenant,
                                    "rpm": 1000000,
                                    "tpm": 1000000,
                                    "priority": 255
                                }
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if index == 0 {
                    StatusCode::OK
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                }
            );
        }
        upstream.assert_async().await;
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn two_http_origins_share_one_redis_tenant_allowance() {
        let Some(redis_url) = std::env::var("LLMSHIM_REDIS_URL").ok() else {
            return;
        };
        let provider_name = format!("tenant-http-{}", uuid::Uuid::new_v4().simple());
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "response",
                    "choices": [{
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {}
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let build_origin = || {
            let router = Arc::new(Router::new().register(
                &provider_name,
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    provider_name.clone(),
                    server.url(),
                    None,
                )),
            ));
            let config = GatewayConfig::default();
            let scheduler = Scheduler::new(
                config.clone(),
                Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
                    crate::proxy::ratelimit::RateLimitConfig::default(),
                )),
                Arc::new(RealDispatch {
                    router: router.clone(),
                    logger: None,
                }),
            );
            let identity = crate::gateway::auth::Identity {
                tenant: "shared-http-tenant".into(),
                tier: 1,
                rpm: Some(1),
                tpm: None,
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            };
            Arc::new(GatewayState {
                router,
                backend: Backend::Local(scheduler),
                keystore: crate::gateway::auth::KeyStore::enforced(
                    std::collections::HashMap::from([("shared-key".into(), identity)]),
                ),
                attempt_coordinator: crate::gateway::attempt::AttemptCoordinator::redis(
                    &redis_url,
                    crate::proxy::ratelimit::RateLimitConfig::with_global(Some(100), None),
                    config.max_concurrency_per_provider,
                    config.max_wait,
                )
                .unwrap(),
                spend: crate::gateway::quota::SpendCap::in_memory(),
                idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                    Duration::from_secs(30),
                ),
                idempotency_ttl_secs: 30,
                overloaded_retry_after: config.overloaded_retry_after,
            })
        };
        let payload = || {
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .header("authorization", "Bearer shared-key")
                .body(Body::from(
                    json!({
                        "model": format!("{provider_name}/test"),
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let first = app(build_origin()).oneshot(payload()).await.unwrap();
        let second = app(build_origin()).oneshot(payload()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        upstream.assert_async().await;
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
    async fn native_idempotency_request_mismatch_is_a_native_shaped_conflict() {
        let mut server = mockito::Server::new_async().await;
        let upstream=server.mock("POST","/chat/completions").with_body(json!({"id":"r","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{}}).to_string()).expect(1).create_async().await;
        let receipts_directory = tempfile::tempdir().unwrap();
        let application = app(configured_state(&server.url())).layer(Extension(Arc::new(
            crate::proxy::wire::Receipts::new(receipts_directory.path().to_owned()),
        )));

        let native_request = |prompt: &str| {
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .header("idempotency-key", "native-client-key")
                .body(Body::from(
                    json!({
                        "model": "local/test",
                        "messages": [{"role": "user", "content": prompt}]
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let first_response = application
            .clone()
            .oneshot(native_request("first"))
            .await
            .unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);

        let conflicting_response = application
            .oneshot(native_request("changed"))
            .await
            .unwrap();
        assert_eq!(conflicting_response.status(), StatusCode::CONFLICT);
        let conflict_body: Value = serde_json::from_slice(
            &to_bytes(conflicting_response.into_body(), 10000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(conflict_body["error"]["type"], "invalid_request_error");
        assert_eq!(conflict_body["error"]["code"], "idempotency_key_reused");

        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn messages_idempotency_mismatch_is_anthropic_shaped_without_cached_content() {
        let mut server = mockito::Server::new_async().await;
        let upstream=server.mock("POST","/chat/completions").with_body(json!({"id":"r","choices":[{"message":{"role":"assistant","content":"private-cached-content"},"finish_reason":"stop"}],"usage":{}}).to_string()).expect(1).create_async().await;
        let receipts_directory = tempfile::tempdir().unwrap();
        let application = app(configured_state(&server.url())).layer(Extension(Arc::new(
            crate::proxy::wire::Receipts::new(receipts_directory.path().to_owned()),
        )));

        let messages_request = |prompt: &str| {
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-api-key", "test-key")
                .header("idempotency-key", "messages-client-key")
                .body(Body::from(
                    json!({
                        "model": "local/test",
                        "max_tokens": 64,
                        "messages": [{"role": "user", "content": prompt}]
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let first_response = application
            .clone()
            .oneshot(messages_request("first"))
            .await
            .unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);

        let conflicting_response = application
            .oneshot(messages_request("changed"))
            .await
            .unwrap();
        assert_eq!(conflicting_response.status(), StatusCode::CONFLICT);
        let conflict_body: Value = serde_json::from_slice(
            &to_bytes(conflicting_response.into_body(), 10000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(conflict_body["type"], "error");
        assert_eq!(conflict_body["error"]["type"], "invalid_request_error");
        assert!(conflict_body["error"].get("code").is_none());
        assert!(!conflict_body.to_string().contains("private-cached-content"));

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

    #[test]
    fn gateway_queues_the_route_expanded_target_and_token_estimate() {
        let router = Router::new()
            .register(
                "vllm",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "vllm",
                    "http://127.0.0.1:9",
                    None,
                )),
            )
            .alias("served-alias", "vllm/served")
            .route(
                "large",
                crate::config::Route {
                    model: "served-alias".into(),
                    settings: std::collections::BTreeMap::from([
                        ("max_tokens".into(), json!(32_000)),
                        (
                            "tools".into(),
                            json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]),
                        ),
                    ]),
                },
            );
        let state = configured_state_with_router(router);
        let headers = HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test-key"),
        )]);
        let request: ChatRequest = serde_json::from_value(json!({
            "model":"route/large",
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let (provider_name, budget_model, gateway_request, _) =
            match build_request(&state, &headers, &request) {
                Ok(prepared) => prepared,
                Err(_) => panic!("route request should be admitted"),
            };
        assert_eq!(provider_name, "vllm");
        assert_eq!(budget_model, "served");
        assert_eq!(gateway_request.provider, "vllm");
        assert_eq!(gateway_request.payload["model"], "served-alias");
        assert_eq!(gateway_request.payload["max_tokens"], 32_000);
        assert!(gateway_request.permits >= 32_000);
    }

    #[test]
    fn gateway_uses_dynamic_openai_compatible_namespace_and_wire_policy() {
        for (wire, prompt_field, output_field) in [
            (
                crate::reasoning::WireFormat::OpenAiChat,
                "messages",
                "max_tokens",
            ),
            (
                crate::reasoning::WireFormat::OpenAiResponses,
                "input",
                "max_output_tokens",
            ),
        ] {
            let router = Router::new().register(
                "local",
                Box::new(
                    crate::providers::openai_compat::OpenAiCompatible::new(
                        "local",
                        "http://127.0.0.1:9",
                        None,
                    )
                    .with_wire(wire),
                ),
            );
            let state = configured_state_with_router(router);
            let headers = HeaderMap::from_iter([(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer test-key"),
            )]);

            for protected_field in ["model", prompt_field] {
                let request: ChatRequest = serde_json::from_value(json!({
                    "model":"local/declared",
                    "messages":[{"role":"user","content":"canonical"}],
                    "provider_config":{"x-local":{(protected_field):"replacement"}}
                }))
                .unwrap();
                assert!(build_request(&state, &headers, &request).is_err());
            }

            let request: ChatRequest = serde_json::from_value(json!({
                "model":"local/declared",
                "messages":[{"role":"user","content":"canonical"}],
                "provider_config":{"x-local":{(output_field):7_000}}
            }))
            .unwrap();
            let (_, _, gateway_request, _) = match build_request(&state, &headers, &request) {
                Ok(prepared) => prepared,
                Err(_) => panic!("native output limit should remain supported"),
            };
            assert!(gateway_request.permits >= 7_000);
        }
    }

    #[tokio::test]
    async fn browser_origin_policy_covers_gateway_and_native_routes() {
        let server = mockito::Server::new_async().await;
        let state = configured_state(&server.url());
        let application = app_with_origin_policy(
            state,
            OriginPolicy::from_csv("https://trusted.example").expect("trusted origin"),
        );

        let denied_native_request = application
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("host", "trusted.example")
                    .header("origin", "https://untrusted.example")
                    .header("content-type", "text/plain")
                    .body(Body::from("browser-simple-request"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_native_request.status(), StatusCode::FORBIDDEN);

        let trusted_native_preflight = application
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/v1/messages")
                    .header("origin", "https://trusted.example")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-type",
                    )
                    .header("access-control-request-private-network", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(trusted_native_preflight.status().is_success());
        assert_eq!(
            trusted_native_preflight
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://trusted.example"
        );
        assert_eq!(
            trusted_native_preflight
                .headers()
                .get("access-control-allow-private-network")
                .unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn gateway_rejects_provider_config_admission_overrides_before_dispatch() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let state = configured_state_for_provider(&server.url(), "local");
        for (path, provider_config) in [
            ("/v1/chat", json!({"model": "local/unpriced"})),
            ("/v1/chat/stream", json!({"x-local": {"messages": []}})),
        ] {
            let response = app(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(
                            json!({
                                "model": "local/test",
                                "messages": [{"role": "user", "content": "canonical"}],
                                "provider_config": provider_config,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let receipt_directory = tempfile::tempdir().unwrap();
        for path in ["/v1/messages", "/v1/chat/completions"] {
            let application = app(state.clone()).layer(Extension(Arc::new(
                crate::proxy::wire::Receipts::new(receipt_directory.path().to_owned()),
            )));
            let response = application
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(
                            json!({
                                "model": "local/test",
                                "messages": [{"role": "user", "content": "canonical"}],
                                "x-local": {"messages": []},
                                "max_tokens": 100
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        upstream.assert_async().await;
    }
}
