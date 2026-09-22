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

use tokio::sync::{mpsc, OwnedSemaphorePermit};

use crate::error::ShimError;
use crate::gateway::{
    ChunkStream, Dispatch, DispatchError, GatewayConfig, GatewayError, GatewayRequest, Scheduler,
    StreamChunk,
};
use crate::log::{Logger, RequestTimer};
use crate::proxy::convert::{chunk_to_events, value_to_response};
use crate::proxy::error::ApiError;
use crate::proxy::ratelimit::{
    build_limiter, estimate_prepared_request_tokens, penalty_duration, Backpressure,
};
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
    fn map_err(
        err: ShimError,
        refusal: Option<crate::policy::AttemptPolicyRefusal>,
    ) -> DispatchError {
        if refusal.is_some_and(|refusal| {
            refusal.kind() == crate::policy::AttemptPolicyRefusalKind::Unpriceable
        }) {
            return DispatchError::new("llmshim-unpriceable-under-budget");
        }
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
            .map_err(|error| Self::map_err(error, None))
    }

    async fn dispatch_with_policy(
        &self,
        _provider: &str,
        payload: Value,
        policy_context: crate::policy::DispatchPolicyContext,
    ) -> Result<Value, DispatchError> {
        let result = crate::completion_with_logger_and_policy(
            self.router.as_ref(),
            &payload,
            self.logger.as_ref(),
            &policy_context,
        )
        .await;
        let refusal = policy_context.take_last_refusal();
        result.map_err(|error| Self::map_err(error, refusal))
    }

    async fn dispatch_stream(
        &self,
        _provider: &str,
        payload: Value,
    ) -> Result<ChunkStream, DispatchError> {
        let upstream = crate::stream(self.router.as_ref(), &payload)
            .await
            .map_err(|error| Self::map_err(error, None))?;
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
        let opened =
            crate::stream_with_policy(self.router.as_ref(), &payload, &policy_context).await;
        let refusal = policy_context.take_last_refusal();
        let upstream = opened.map_err(|error| Self::map_err(error, refusal))?;
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

enum PreparedGatewaySubmission {
    Local {
        request: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
    },
    #[cfg(feature = "redis-coordination")]
    Distributed(crate::gateway::distributed::PreparedSubmission),
}

struct SubmittedValue {
    value: Value,
    #[cfg(feature = "redis-coordination")]
    lifecycle_reference: Option<crate::gateway::distributed::LifecycleReference>,
}

/// Shared state for the gateway HTTP handlers.
pub struct GatewayState {
    router: Arc<Router>,
    backend: Backend,
    keystore: crate::gateway::auth::KeyStore,
    attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
    idempotency: crate::gateway::idempotency::IdempotencyCache,
    #[cfg_attr(not(feature = "redis-coordination"), allow(dead_code))]
    idempotency_ttl_secs: u64,
    prequeue_backpressure: Backpressure,
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
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idem_ttl_secs()),
            ),
            idempotency_ttl_secs: idem_ttl_secs(),
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
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
        Ok(Arc::new(Self {
            router,
            backend: Backend::Distributed(gateway),
            keystore: crate::gateway::auth::KeyStore::from_env(),
            attempt_coordinator,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idem_ttl_secs()),
            ),
            idempotency_ttl_secs: idem_ttl_secs(),
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
            overloaded_retry_after: config.overloaded_retry_after,
        }))
    }

    async fn trusted_policy_scope(
        &self,
        identity: &crate::gateway::auth::Identity,
    ) -> Result<crate::gateway::attempt::TrustedPolicyScope, GatewayError> {
        let policy_scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(identity);
        #[cfg(feature = "redis-coordination")]
        let mut policy_scope = policy_scope;
        #[cfg(feature = "redis-coordination")]
        if identity.budget_usd.is_some() {
            if let Backend::Distributed(gateway) = &self.backend {
                let _ = gateway;
                self.attempt_coordinator
                    .retain_legacy_spend_floor(&identity.tenant, &mut policy_scope)
                    .await
                    .map_err(|_| {
                        GatewayError::Upstream("llmshim-coordinator-unavailable".into())
                    })?;
            }
        }
        Ok(policy_scope)
    }

    fn prepare_submission(
        &self,
        req: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
        stream: bool,
    ) -> Result<PreparedGatewaySubmission, GatewayError> {
        #[cfg(not(feature = "redis-coordination"))]
        let _ = stream;
        match &self.backend {
            Backend::Local(_) => Ok(PreparedGatewaySubmission::Local {
                request: req,
                policy_scope,
            }),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway
                .prepare_with_policy(req, stream, policy_scope)
                .map(PreparedGatewaySubmission::Distributed),
        }
    }

    async fn submit_prepared(
        &self,
        prepared: PreparedGatewaySubmission,
        prequeue_permit: PrequeuePreparationPermit,
        logical_deadline: Option<tokio::time::Instant>,
    ) -> Result<SubmittedValue, GatewayError> {
        match prepared {
            PreparedGatewaySubmission::Local {
                request,
                policy_scope,
            } => match &self.backend {
                Backend::Local(scheduler) => {
                    drop(prequeue_permit);
                    let policy_context = self.attempt_coordinator.context(policy_scope);
                    let value = match logical_deadline {
                        Some(deadline) => {
                            scheduler
                                .submit_with_policy_deadline(request, policy_context, deadline)
                                .await
                        }
                        None => scheduler.submit_with_policy(request, policy_context).await,
                    }?;
                    Ok(SubmittedValue {
                        value,
                        #[cfg(feature = "redis-coordination")]
                        lifecycle_reference: None,
                    })
                }
                #[cfg(feature = "redis-coordination")]
                Backend::Distributed(_) => unreachable!("prepared backend changed"),
            },
            #[cfg(feature = "redis-coordination")]
            PreparedGatewaySubmission::Distributed(prepared) => match &self.backend {
                Backend::Distributed(gateway) => {
                    drop(prequeue_permit);
                    gateway
                        .submit_prepared_with_reference(prepared, logical_deadline)
                        .await
                        .map(|(value, lifecycle_reference)| SubmittedValue {
                            value,
                            lifecycle_reference: Some(lifecycle_reference),
                        })
                }
                Backend::Local(_) => unreachable!("prepared backend changed"),
            },
        }
    }

    async fn submit_stream_prepared(
        &self,
        prepared: PreparedGatewaySubmission,
        prequeue_permit: PrequeuePreparationPermit,
        logical_deadline: Option<tokio::time::Instant>,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        match prepared {
            PreparedGatewaySubmission::Local {
                request,
                policy_scope,
            } => match &self.backend {
                Backend::Local(scheduler) => {
                    drop(prequeue_permit);
                    let policy_context = self.attempt_coordinator.context(policy_scope);
                    match logical_deadline {
                        Some(deadline) => {
                            scheduler
                                .submit_stream_with_policy_deadline(
                                    request,
                                    policy_context,
                                    deadline,
                                )
                                .await
                        }
                        None => {
                            scheduler
                                .submit_stream_with_policy(request, policy_context)
                                .await
                        }
                    }
                }
                #[cfg(feature = "redis-coordination")]
                Backend::Distributed(_) => unreachable!("prepared backend changed"),
            },
            #[cfg(feature = "redis-coordination")]
            PreparedGatewaySubmission::Distributed(prepared) => match &self.backend {
                Backend::Distributed(gateway) => {
                    let accepted = gateway.accept_prepared(prepared, logical_deadline).await?;
                    drop(prequeue_permit);
                    Ok(gateway.stream_accepted(accepted))
                }
                Backend::Local(_) => unreachable!("prepared backend changed"),
            },
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
        #[cfg(feature = "redis-coordination")] lifecycle_reference: Option<
            &crate::gateway::distributed::LifecycleReference,
        >,
    ) {
        match &self.backend {
            Backend::Local(_) => self.idempotency.store(context, value.clone()),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => {
                if let Some(lifecycle_reference) = lifecycle_reference {
                    gateway
                        .scoped_idem_store(context, lifecycle_reference, self.idempotency_ttl_secs)
                        .await
                }
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
        GatewayError::Upstream(message) if message == "llmshim-unpriceable-under-budget" => {
            ApiError::from(ShimError::ProviderError {
                status: 400,
                body: json!({
                    "error": {
                        "message": "the selected request has no enforceable spend bound",
                        "type": "invalid_request_error",
                        "param": "model",
                        "code": "unpriceable_under_budget"
                    }
                })
                .to_string(),
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
    installed_prequeue_permit: Option<axum::Extension<PrequeuePreparationPermit>>,
    logical_lifetime: Option<axum::Extension<crate::proxy::lifetime::LogicalRequestLifetime>>,
    headers: HeaderMap,
    uri: Uri,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    let prequeue_permit = prequeue_permit(&state, installed_prequeue_permit).await?;
    if logical_lifetime
        .as_ref()
        .is_some_and(|axum::Extension(lifetime)| lifetime.select_streaming(req.stream).is_err())
    {
        return Ok(crate::proxy::lifetime::timeout_response(uri.path()));
    }
    let logical_deadline = logical_lifetime
        .as_ref()
        .map(|axum::Extension(lifetime)| lifetime.deadline());
    if req.stream {
        return Ok(chat_stream_inner(state, headers, req, prequeue_permit, logical_lifetime).await);
    }

    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (provider_name, _budget_model, gw, identified_caller) =
        build_request(&state, &headers, &req)?;
    let identity = &identified_caller.identity;
    let idempotency_context = idem_key.as_ref().map(|client_key| {
        crate::gateway::idempotency::IdempotencyContext::new(
            &identity.tenant,
            &identified_caller.credential_scope,
            uri.path(),
            client_key,
            &gw.payload,
        )
    });
    let policy_scope = state
        .trusted_policy_scope(identity)
        .await
        .map_err(|error| gateway_err_to_api(&state, error))?;
    let prepared_submission = state
        .prepare_submission(gw, policy_scope, false)
        .map_err(|error| gateway_err_to_api(&state, error))?;

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
    match state
        .submit_prepared(prepared_submission, prequeue_permit, logical_deadline)
        .await
    {
        Ok(submitted) => {
            if let Some(context) = &idempotency_context {
                state
                    .idem_store(
                        context,
                        &submitted.value,
                        #[cfg(feature = "redis-coordination")]
                        submitted.lifecycle_reference.as_ref(),
                    )
                    .await;
            }
            let elapsed = timer.elapsed().as_millis() as u64;
            Ok(Json(value_to_response(&submitted.value, &provider_name, elapsed)).into_response())
        }
        Err(err) => Err(gateway_err_to_api(&state, err)),
    }
}

/// POST /v1/chat/stream — always SSE, queued by priority.
async fn chat_stream(
    State(state): State<Arc<GatewayState>>,
    installed_prequeue_permit: Option<axum::Extension<PrequeuePreparationPermit>>,
    logical_lifetime: Option<axum::Extension<crate::proxy::lifetime::LogicalRequestLifetime>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    let prequeue_permit = match prequeue_permit(&state, installed_prequeue_permit).await {
        Ok(permit) => permit,
        Err(error) => return error.into_response(),
    };
    if logical_lifetime
        .as_ref()
        .is_some_and(|axum::Extension(lifetime)| lifetime.select_streaming(true).is_err())
    {
        return crate::proxy::lifetime::timeout_response("/v1/chat/stream");
    }
    chat_stream_inner(state, headers, req, prequeue_permit, logical_lifetime).await
}

async fn chat_stream_inner(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    req: ChatRequest,
    prequeue_permit: PrequeuePreparationPermit,
    logical_lifetime: Option<axum::Extension<crate::proxy::lifetime::LogicalRequestLifetime>>,
) -> Response {
    let (_provider_name, _budget_model, gw, identified_caller) =
        match build_request(&state, &headers, &req) {
            Ok(t) => t,
            Err(e) => return e.into_response(),
        };
    let identity = identified_caller.identity;
    let policy_scope = match state.trusted_policy_scope(&identity).await {
        Ok(scope) => scope,
        Err(error) => return gateway_err_to_api(&state, error).into_response(),
    };
    let prepared_submission = match state.prepare_submission(gw, policy_scope, true) {
        Ok(prepared) => prepared,
        Err(error) => return gateway_err_to_api(&state, error).into_response(),
    };
    // Admission (queue + rate) happens up front so a rejection is a proper
    // 429/503 before the SSE response begins, not an SSE error event.
    let mut rx = match state
        .submit_stream_prepared(
            prepared_submission,
            prequeue_permit,
            logical_lifetime
                .as_ref()
                .map(|axum::Extension(lifetime)| lifetime.deadline()),
        )
        .await
    {
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
    app_with_origin_policy_and_deadlines(
        state,
        origin_policy,
        crate::proxy::lifetime::LogicalRequestDeadlines::gateway_from_env(),
    )
}

fn app_with_origin_policy_and_deadlines(
    state: Arc<GatewayState>,
    origin_policy: OriginPolicy,
    logical_deadlines: crate::proxy::lifetime::LogicalRequestDeadlines,
) -> axum::Router {
    let default_receipt_store = crate::proxy::wire::DefaultReceiptStore::from_env();
    let ingress_state = IngressAdmissionState {
        gateway: state.clone(),
        deadlines: logical_deadlines,
    };
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
        .layer(axum::middleware::from_fn(
            crate::proxy::wire::bound_inference_request_json,
        ))
        .layer(axum::middleware::from_fn_with_state(
            default_receipt_store,
            crate::proxy::wire::install_default_receipt_store,
        ))
        .layer(axum::middleware::from_fn_with_state(
            ingress_state,
            admit_ingress_preparation,
        ))
        .layer(axum::middleware::from_fn(request_id))
        .layer(origin_policy.cors_layer())
        .layer(axum::middleware::from_fn(move |request, next| {
            crate::proxy::origin::admit_browser_origin(origin_policy.clone(), request, next)
        }))
        .with_state(state)
}

#[derive(Clone)]
struct PrequeuePreparationPermit {
    _permit: Arc<OwnedSemaphorePermit>,
}

#[derive(Clone)]
struct IngressAdmissionState {
    gateway: Arc<GatewayState>,
    deadlines: crate::proxy::lifetime::LogicalRequestDeadlines,
}

async fn prequeue_permit(
    state: &GatewayState,
    installed_permit: Option<axum::Extension<PrequeuePreparationPermit>>,
) -> Result<PrequeuePreparationPermit, ApiError> {
    match installed_permit {
        Some(axum::Extension(permit)) => Ok(permit),
        None => state
            .prequeue_backpressure
            .acquire_preparation()
            .await
            .map(|permit| PrequeuePreparationPermit {
                _permit: Arc::new(permit),
            })
            .map_err(|_| ApiError::Overloaded(state.prequeue_backpressure.queue_timeout())),
    }
}

async fn admit_ingress_preparation(
    State(ingress): State<IngressAdmissionState>,
    mut request: Request,
    next: Next,
) -> Response {
    let state = &ingress.gateway;
    let inference_path = request.uri().path().to_owned();
    let requires_preparation = request.method() == axum::http::Method::POST
        && matches!(
            inference_path.as_str(),
            "/v1/chat" | "/v1/chat/stream" | "/v1/chat/completions" | "/v1/messages"
        );
    if !requires_preparation {
        return next.run(request).await;
    }

    let native_wire = match inference_path.as_str() {
        "/v1/chat/completions" => Some(crate::proxy::wire::Wire::Chat),
        "/v1/messages" => Some(crate::proxy::wire::Wire::Messages),
        _ => None,
    };
    if native_wire.is_some() {
        crate::proxy::wire::normalize_auth(request.headers_mut());
    }
    if state.keystore.identify(request.headers()).is_err() {
        return match native_wire {
            Some(wire) => crate::proxy::wire::fail(
                wire,
                StatusCode::UNAUTHORIZED,
                "Missing or invalid API key",
            ),
            None => ApiError::Unauthorized.into_response(),
        };
    }

    let permit = match state.prequeue_backpressure.acquire_preparation().await {
        Ok(permit) => PrequeuePreparationPermit {
            _permit: Arc::new(permit),
        },
        Err(()) => {
            return match native_wire {
                Some(wire) => {
                    let mut response = crate::proxy::wire::fail(
                        wire,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Gateway is at capacity; retry after the suggested delay",
                    );
                    insert_retry_after(&mut response, state.prequeue_backpressure.queue_timeout());
                    response
                }
                None => ApiError::Overloaded(state.prequeue_backpressure.queue_timeout())
                    .into_response(),
            }
        }
    };
    request.extensions_mut().insert(permit);
    let lifetime =
        crate::proxy::lifetime::LogicalRequestLifetime::new(&inference_path, ingress.deadlines);
    request.extensions_mut().insert(lifetime.clone());
    let response_future = next.run(request);
    tokio::pin!(response_future);
    let response = tokio::select! {
        biased;
        _ = lifetime.expired() => {
            return crate::proxy::lifetime::timeout_response(&inference_path)
        }
        response = &mut response_future => response,
    };
    if lifetime.is_expired() {
        drop(response);
        return crate::proxy::lifetime::timeout_response(&inference_path);
    }
    crate::proxy::lifetime::pump_response(response, inference_path, lifetime)
}

fn insert_retry_after(response: &mut Response, wait: Duration) {
    let seconds = wait.as_secs() + u64::from(wait.subsec_millis() > 0);
    if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
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
        configured_state_with_router_and_identity(
            router,
            crate::gateway::auth::Identity {
                tenant: "test-tenant".into(),
                tier: 1,
                rpm: requests_per_minute,
                tpm: tokens_per_minute,
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            },
        )
    }

    fn configured_state_with_router_and_identity(
        router: Router,
        identity: crate::gateway::auth::Identity,
    ) -> Arc<GatewayState> {
        let config = GatewayConfig::default();
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::local(
            crate::proxy::ratelimit::RateLimitConfig::default(),
            config.max_concurrency_per_provider,
            config.max_wait,
        );
        configured_state_with_router_identity_and_coordinator(router, identity, attempt_coordinator)
    }

    fn configured_state_with_router_identity_and_coordinator(
        router: Router,
        identity: crate::gateway::auth::Identity,
        attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
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
        let keys = std::collections::HashMap::from([
            ("test-key".into(), identity.clone()),
            ("second-key".into(), identity),
        ]);
        Arc::new(GatewayState {
            router,
            backend: Backend::Local(scheduler),
            keystore: crate::gateway::auth::KeyStore::enforced(keys),
            attempt_coordinator,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
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
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
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

    #[tokio::test]
    async fn ingress_preparation_gate_precedes_native_conversion_and_prequeue_work() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "keyed-ingress-response",
                    "model": "test",
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
        let mut state = configured_state(&server.url());
        Arc::get_mut(&mut state)
            .expect("test owns the only gateway state reference")
            .prequeue_backpressure = Backpressure::new(1, Duration::from_millis(20));
        let held_prequeue_permit = state
            .prequeue_backpressure
            .acquire_preparation()
            .await
            .expect("hold the only ingress preparation permit");

        let unauthorized = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from("not-json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let unauthorized_body: Value =
            serde_json::from_slice(&to_bytes(unauthorized.into_body(), 10_000).await.unwrap())
                .unwrap();
        assert_eq!(unauthorized_body["error"]["type"], "authentication_error");

        assert_gateway_refusal_surfaces(state.clone(), true).await;
        drop(held_prequeue_permit);

        let released = tokio::time::timeout(
            Duration::from_secs(1),
            app(state).oneshot(canonical_request("test-key", "keyed-ingress", "hi")),
        )
        .await
        .expect("released prequeue capacity should not stall the next request")
        .unwrap();
        assert_eq!(released.status(), StatusCode::OK);
        upstream.assert_async().await;
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
    struct QuietBodyDropDispatch {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<tokio::sync::Notify>,
    }

    #[cfg(feature = "redis-coordination")]
    struct QuietBodyDropGuard(Arc<tokio::sync::Notify>);

    #[cfg(feature = "redis-coordination")]
    impl Drop for QuietBodyDropGuard {
        fn drop(&mut self) {
            self.0.notify_one();
        }
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
    #[async_trait::async_trait]
    impl Dispatch for QuietBodyDropDispatch {
        async fn dispatch(&self, _provider: &str, _payload: Value) -> Result<Value, DispatchError> {
            unreachable!("quiet body-drop fixture only streams")
        }

        async fn dispatch_stream(
            &self,
            _provider: &str,
            _payload: Value,
        ) -> Result<crate::gateway::ChunkStream, DispatchError> {
            let started = self.started.clone();
            let dropped = self.dropped.clone();
            Ok(Box::pin(async_stream::stream! {
                let _guard = QuietBodyDropGuard(dropped);
                started.notify_one();
                std::future::pending::<()>().await;
                yield Ok(String::new());
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
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
            overloaded_retry_after: config.overloaded_retry_after,
        })
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_quiet_final_http_body_drop_cancels_distributed_dispatch() {
        use redis::AsyncCommands;

        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let namespace = uuid::Uuid::new_v4();
        let provider = format!("body-drop-{namespace}");
        let router = Arc::new(Router::new().register(
            &provider,
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                provider.clone(),
                "http://127.0.0.1:1",
                None,
            )),
        ));
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(tokio::sync::Notify::new());
        let config = GatewayConfig {
            lease_timeout: Duration::from_millis(90),
            request_timeout: Duration::from_secs(2),
            ..GatewayConfig::default()
        };
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::redis(
            &redis_url,
            crate::proxy::ratelimit::RateLimitConfig::default(),
            config.max_concurrency_per_provider,
            config.max_wait,
        )
        .unwrap();
        let gateway = crate::gateway::distributed::DistributedGateway::connect_with_coordinator(
            &redis_url,
            Arc::new(QuietBodyDropDispatch {
                started: started.clone(),
                dropped: dropped.clone(),
            }),
            Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
                crate::proxy::ratelimit::RateLimitConfig::default(),
            )),
            config.clone(),
            attempt_coordinator.clone(),
        )
        .await
        .unwrap();
        gateway.spawn_workers(vec![provider.clone()]);
        let application = app(distributed_state_for_tenant(
            router,
            gateway.clone(),
            &config,
            attempt_coordinator,
            "body-drop-tenant",
        ));
        let started_notification = started.notified();
        let response = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer reassigned-key")
                    .body(Body::from(
                        json!({
                            "model": format!("{provider}/test"),
                            "messages": [{"role":"user","content":"quiet"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(1), started_notification)
            .await
            .expect("distributed stream should start");
        let mut observer = gateway.connection_for_test().await.unwrap();
        let processing_key = format!("llmshim:gw:lifecycle:v2:proc:scoped:{provider}");
        assert_eq!(observer.zcard::<_, u64>(&processing_key).await.unwrap(), 1);
        let dropped_notification = dropped.notified();
        drop(response);
        tokio::time::timeout(Duration::from_secs(1), dropped_notification)
            .await
            .expect("dropping final body should cancel the quiet provider stream");
        tokio::time::timeout(Duration::from_secs(1), async {
            while observer.zcard::<_, u64>(&processing_key).await.unwrap() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled delivery should leave processing");
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

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn authenticated_scope_imports_legacy_spend_without_serializing_the_tenant() {
        use redis::AsyncCommands;

        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let tenant = format!("legacy-http-{}", uuid::Uuid::new_v4());
        let window_secs = 3_600;
        let config = GatewayConfig::default();
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
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
                crate::proxy::ratelimit::RateLimitConfig::default(),
            )),
            config.clone(),
            attempt_coordinator.clone(),
        )
        .await
        .unwrap();
        let identity = crate::gateway::auth::Identity {
            tenant: tenant.clone(),
            tier: 1,
            rpm: None,
            tpm: None,
            budget_usd: Some(1.0),
            budget_window_secs: Some(window_secs),
            budget_allow_unpriced: false,
        };
        let mut connection = gateway.connection_for_test().await.unwrap();
        let redis_time: (u64, u64) = redis::cmd("TIME")
            .query_async(&mut connection)
            .await
            .unwrap();
        let window_index = redis_time.0 / window_secs;
        let legacy_key = format!("llmshim:spend:{tenant}:{window_index}");
        let _: () = connection.set(&legacy_key, "0.00000005").await.unwrap();
        let state = GatewayState {
            router: Arc::new(Router::new()),
            backend: Backend::Distributed(gateway),
            keystore: crate::gateway::auth::KeyStore::Open,
            attempt_coordinator,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
            overloaded_retry_after: config.overloaded_retry_after,
        };
        let serialized =
            serde_json::to_string(&state.trusted_policy_scope(&identity).await.unwrap()).unwrap();
        assert!(!serialized.contains(&tenant));
        let value: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["legacy_spend_floor"]["window_index"], window_index);
        assert_eq!(value["legacy_spend_floor"]["amount_nanos"], 50);
        let tenant_key = value["tenant_key"].as_str().unwrap();
        let known_floor_key = format!(
            "llmshim:gw:budget:v1:{tenant_key}:{window_secs}:{window_index}:legacy-known-floor"
        );
        assert_eq!(
            connection.get::<_, u64>(&known_floor_key).await.unwrap(),
            50
        );
        let _: i64 = connection
            .zrem(
                crate::gateway::attempt::REDIS_ACCOUNTING_INDEX_KEY,
                &known_floor_key,
            )
            .await
            .unwrap();
        let _: i64 = connection.del((legacy_key, known_floor_key)).await.unwrap();
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn authenticated_replay_retains_fresh_legacy_floor_without_new_attempt() {
        use redis::AsyncCommands;
        use sha2::Digest;

        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let namespace = uuid::Uuid::new_v4();
        let tenant = format!("origin-replay-{namespace}");
        let provider_name = format!("origin-replay-{namespace}");
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
            Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
                crate::proxy::ratelimit::RateLimitConfig::default(),
            )),
            config.clone(),
            attempt_coordinator.clone(),
        )
        .await
        .unwrap();
        gateway.spawn_workers(vec![provider_name.clone()]);
        let identity = crate::gateway::auth::Identity {
            tenant: tenant.clone(),
            tier: 1,
            rpm: None,
            tpm: None,
            budget_usd: Some(1.0),
            budget_window_secs: Some(3_600),
            budget_allow_unpriced: true,
        };
        let state = Arc::new(GatewayState {
            router,
            backend: Backend::Distributed(gateway.clone()),
            keystore: crate::gateway::auth::KeyStore::enforced(std::collections::HashMap::from([
                ("origin-key".into(), identity),
            ])),
            attempt_coordinator,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(Duration::from_secs(
                30,
            )),
            idempotency_ttl_secs: 30,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
            overloaded_retry_after: config.overloaded_retry_after,
        });
        let application = app(state);
        let mut connection = gateway.connection_for_test().await.unwrap();
        let redis_time: (u64, u64) = redis::cmd("TIME")
            .query_async(&mut connection)
            .await
            .unwrap();
        let window_index = redis_time.0 / 3_600;
        let legacy_key = format!("llmshim:spend:{tenant}:{window_index}");
        let model = format!("{provider_name}/test");
        let idempotency_key = format!("origin-replay-{namespace}");

        let _: () = connection.set(&legacy_key, "0.00000005").await.unwrap();
        let first_response = application
            .clone()
            .oneshot(canonical_request_for_model(
                "origin-key",
                &idempotency_key,
                "same-request",
                &model,
            ))
            .await
            .unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let tenant_key = format!("{:x}", sha2::Sha256::digest(tenant.as_bytes()));
        let scoped_members_before_replay: Vec<String> = connection
            .zrange(crate::gateway::attempt::REDIS_ACCOUNTING_INDEX_KEY, 0, -1)
            .await
            .unwrap();
        let scoped_members_before_replay: Vec<String> = scoped_members_before_replay
            .into_iter()
            .filter(|member| member.contains(&tenant_key))
            .collect();
        assert_eq!(scoped_members_before_replay.len(), 1);

        let _: () = connection.set(&legacy_key, "0.00000009").await.unwrap();
        let replay_response = application
            .oneshot(canonical_request_for_model(
                "origin-key",
                &idempotency_key,
                "same-request",
                &model,
            ))
            .await
            .unwrap();
        assert_eq!(replay_response.status(), StatusCode::OK);
        assert_eq!(replay_response.headers()["idempotency-replayed"], "true");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let total_key = format!("llmshim:gw:budget:v1:{tenant_key}:3600:{window_index}");
        assert_eq!(
            connection
                .get::<_, u64>(format!("{total_key}:legacy-known-floor"))
                .await
                .unwrap(),
            90
        );
        assert!(!connection.exists::<_, bool>(&total_key).await.unwrap());
        assert!(!connection
            .exists::<_, bool>(format!("{total_key}:legacy-floor"))
            .await
            .unwrap());
        let scoped_members_after_replay: Vec<String> = connection
            .zrange(crate::gateway::attempt::REDIS_ACCOUNTING_INDEX_KEY, 0, -1)
            .await
            .unwrap();
        let scoped_members_after_replay: Vec<String> = scoped_members_after_replay
            .into_iter()
            .filter(|member| member.contains(&tenant_key))
            .collect();
        assert_eq!(scoped_members_after_replay, scoped_members_before_replay);

        for member in scoped_members_after_replay {
            let _: usize = connection
                .zrem(crate::gateway::attempt::REDIS_ACCOUNTING_INDEX_KEY, &member)
                .await
                .unwrap();
            let _: i64 = connection.del(member).await.unwrap();
        }
        let _: i64 = connection
            .del((
                legacy_key,
                total_key.clone(),
                format!("{total_key}:legacy-floor"),
                format!("{total_key}:legacy-known-floor"),
            ))
            .await
            .unwrap();
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

    fn budgeted_identity(limit_usd: f64, allow_unpriced: bool) -> crate::gateway::auth::Identity {
        crate::gateway::auth::Identity {
            tenant: format!("budget-test-{}", uuid::Uuid::new_v4()),
            tier: 1,
            rpm: None,
            tpm: None,
            budget_usd: Some(limit_usd),
            budget_window_secs: Some(3600),
            budget_allow_unpriced: allow_unpriced,
        }
    }

    async fn assert_chat_stream_budget_finality(
        events: Vec<Value>,
        expected_second_status: StatusCode,
    ) {
        let mut server = mockito::Server::new_async().await;
        let mut stream_body = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        stream_body.push_str("data: [DONE]\n\n");
        let expected_calls = usize::from(expected_second_status == StatusCode::OK) + 1;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_header("content-type", "text/event-stream")
            .with_body(stream_body)
            .expect(expected_calls)
            .create_async()
            .await;
        let router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(server.url()),
            ),
        );
        let application = app(configured_state_with_router_and_identity(
            router,
            budgeted_identity(10.0, false),
        ));
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/chat/stream")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .body(Body::from(
                    json!({
                        "model": "openrouter/x-ai/grok-4.7",
                        "messages": [{"role": "user", "content": "hi"}],
                        "stream": true
                    })
                    .to_string(),
                ))
                .unwrap()
        };
        let first = application.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let _ = to_bytes(first.into_body(), 100_000).await.unwrap();
        let second = application.oneshot(request()).await.unwrap();
        assert_eq!(second.status(), expected_second_status);
        if second.status() == StatusCode::OK {
            let _ = to_bytes(second.into_body(), 100_000).await.unwrap();
        }
        upstream.assert_async().await;
    }

    async fn assert_gemini_stream_budget_finality(
        events: Vec<Value>,
        expected_second_status: StatusCode,
    ) {
        let mut server = mockito::Server::new_async().await;
        let stream_body = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let expected_calls = usize::from(expected_second_status == StatusCode::OK) + 1;
        let upstream = server
            .mock("POST", Matcher::Regex("/models/.*".into()))
            .with_header("content-type", "text/event-stream")
            .with_body(stream_body)
            .expect(expected_calls)
            .create_async()
            .await;
        let router = Router::new().register(
            "gemini",
            Box::new(
                crate::providers::gemini::Gemini::new("test-key".into())
                    .with_base_url(server.url()),
            ),
        );
        let application = app(configured_state_with_router_and_identity(
            router,
            budgeted_identity(10.0, true),
        ));
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/chat/stream")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .body(Body::from(
                    json!({
                        "model": "gemini/gemini-3.8-flash",
                        "messages": [{"role": "user", "content": "hi"}],
                        "stream": true
                    })
                    .to_string(),
                ))
                .unwrap()
        };
        let first = application.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let _ = to_bytes(first.into_body(), 100_000).await.unwrap();
        let second = application.oneshot(request()).await.unwrap();
        assert_eq!(second.status(), expected_second_status);
        if second.status() == StatusCode::OK {
            let _ = to_bytes(second.into_body(), 100_000).await.unwrap();
        }
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn budget_rejects_unknown_and_surcharged_requests_before_send() {
        let mut unknown_server = mockito::Server::new_async().await;
        let unknown_upstream = unknown_server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let unknown_router = Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local",
                unknown_server.url(),
                None,
            )),
        );
        let unknown_response = app(configured_state_with_router_and_identity(
            unknown_router,
            budgeted_identity(100.0, false),
        ))
        .oneshot(refusal_request("/v1/chat", false))
        .await
        .unwrap();
        assert_eq!(unknown_response.status(), StatusCode::BAD_REQUEST);
        unknown_upstream.assert_async().await;

        let mut surcharge_server = mockito::Server::new_async().await;
        let surcharge_upstream = surcharge_server
            .mock("POST", "/responses")
            .expect(0)
            .create_async()
            .await;
        let surcharge_router = Router::new().register(
            "openai",
            Box::new(
                crate::providers::openai::OpenAi::new("test-key".into())
                    .with_base_url(surcharge_server.url()),
            ),
        );
        let surcharge_state = configured_state_with_router_and_identity(
            surcharge_router,
            budgeted_identity(100.0, false),
        );
        let unverified_response = app(surcharge_state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(
                        json!({
                            "model": "openai/gpt-5.6-luna",
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unverified_response.status(), StatusCode::BAD_REQUEST);
        let surcharge_request = Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(
                json!({
                    "model": "openai/gpt-5.6-luna",
                    "messages": [{"role": "user", "content": "hi"}],
                    "provider_config": {"speed": "fast"}
                })
                .to_string(),
            ))
            .unwrap();
        let surcharge_response = app(surcharge_state)
            .oneshot(surcharge_request)
            .await
            .unwrap();
        assert_eq!(surcharge_response.status(), StatusCode::BAD_REQUEST);
        surcharge_upstream.assert_async().await;

        let mut cache_server = mockito::Server::new_async().await;
        let cache_upstream = cache_server
            .mock("POST", "/messages")
            .expect(0)
            .create_async()
            .await;
        let cache_router = Router::new().register(
            "anthropic",
            Box::new(
                crate::providers::anthropic::Anthropic::new("test-key".into())
                    .with_base_url(cache_server.url()),
            ),
        );
        let cache_request = Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(
                json!({
                    "model": "anthropic/claude-sonnet-4-6",
                    "messages": [{
                        "role": "user",
                        "content": [{
                            "type": "text",
                            "text": "hi",
                            "cache_control": {"type": "ephemeral", "ttl": "1h"}
                        }]
                    }]
                })
                .to_string(),
            ))
            .unwrap();
        let cache_response = app(configured_state_with_router_and_identity(
            cache_router,
            budgeted_identity(100.0, false),
        ))
        .oneshot(cache_request)
        .await
        .unwrap();
        assert_eq!(cache_response.status(), StatusCode::BAD_REQUEST);
        cache_upstream.assert_async().await;

        let mut openrouter_server = mockito::Server::new_async().await;
        let openrouter_upstream = openrouter_server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let openrouter_router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(openrouter_server.url()),
            ),
        );
        let openrouter_state = configured_state_with_router_and_identity(
            openrouter_router,
            budgeted_identity(100.0, false),
        );
        for provider_config in [
            json!({"n": 2}),
            json!({"x-openrouter": {"route": "fallback"}}),
        ] {
            let response = app(openrouter_state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/chat")
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(
                            json!({
                                "model": "openrouter/x-ai/grok-4.7",
                                "messages": [{"role": "user", "content": "hi"}],
                                "provider_config": provider_config
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let mutable_route = app(openrouter_state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(
                        json!({
                            "model": "openrouter/openrouter/free",
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mutable_route.status(), StatusCode::BAD_REQUEST);
        openrouter_upstream.assert_async().await;

        let mut gemini_server = mockito::Server::new_async().await;
        let gemini_upstream = gemini_server
            .mock("POST", Matcher::Regex("/models/.*".into()))
            .expect(0)
            .create_async()
            .await;
        let gemini_router = Router::new().register(
            "gemini",
            Box::new(
                crate::providers::gemini::Gemini::new("test-key".into())
                    .with_base_url(gemini_server.url()),
            ),
        );
        let gemini_response = app(configured_state_with_router_and_identity(
            gemini_router,
            budgeted_identity(100.0, false),
        ))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .body(Body::from(
                    json!({
                        "model": "gemini/gemini-3.8-flash",
                        "messages": [{
                            "role": "user",
                            "content": [{
                                "type": "image_url",
                                "image_url": {"url": "data:image/png;base64,AA=="}
                            }]
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(gemini_response.status(), StatusCode::BAD_REQUEST);
        gemini_upstream.assert_async().await;
    }

    #[tokio::test]
    async fn terminal_failed_repair_settles_both_provider_responses_before_502() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "response",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "1"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
                })
                .to_string(),
            )
            .expect(2)
            .create_async()
            .await;
        let router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(server.url()),
            ),
        );
        let identity = budgeted_identity(100.0, false);
        let scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(&identity);
        let state = configured_state_with_router_and_identity(router, identity);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(
                json!({
                    "model": "openrouter/x-ai/grok-4.7",
                    "messages": [{"role": "user", "content": "answer"}],
                    "response_format": {
                        "type": "json_schema",
                        "json_schema": {
                            "name": "answer",
                            "schema": {"type": "integer", "minimum": 3}
                        }
                    },
                    "x-shim": {"structured_output": "prompt"}
                })
                .to_string(),
            ))
            .unwrap();

        let response = app(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        upstream.assert_async().await;
        let charged_nanos = state
            .attempt_coordinator
            .budget_total_for_test(&scope)
            .await
            .unwrap();
        assert!(
            charged_nanos > 0,
            "known usage from both attempts is charged"
        );
        let one_attempt_usd = crate::cost::cost_usd(
            "openrouter",
            "x-ai/grok-4.7",
            &json!({"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}),
        )
        .unwrap();
        assert_eq!(
            charged_nanos,
            (one_attempt_usd * 1_000_000_000.0).ceil() as u64 * 2
        );
    }

    #[tokio::test]
    async fn semaphore_wait_crossing_short_windows_quotes_at_actual_acquisition() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "response",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }]
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(server.url()),
            ),
        );
        let mut identity = budgeted_identity(10.0, false);
        identity.budget_window_secs = Some(1);
        let coordinator = crate::gateway::attempt::AttemptCoordinator::local(
            crate::proxy::ratelimit::RateLimitConfig::default(),
            1,
            Duration::from_secs(5),
        );
        let held = coordinator.hold_provider_for_test("openrouter").await;
        let state =
            configured_state_with_router_identity_and_coordinator(router, identity, coordinator);
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .body(Body::from(
                    json!({
                        "model": "openrouter/x-ai/grok-4.7",
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap()
        };
        let application = app(state);
        let waiting = tokio::spawn(application.clone().oneshot(request()));
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        let subsecond = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_millis() as u64;
        tokio::time::sleep(Duration::from_millis(1_020 - subsecond)).await;
        drop(held);

        let first = waiting.await.unwrap().unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = application.oneshot(request()).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn explicit_zero_releases_only_a_verified_bounded_quote() {
        let response_body = json!({
            "id": "response",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0}
        })
        .to_string();
        let request_for = |model: &str| {
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .body(Body::from(
                    json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let mut bounded_server = mockito::Server::new_async().await;
        let bounded_upstream = bounded_server
            .mock("POST", "/chat/completions")
            .with_body(&response_body)
            .expect(2)
            .create_async()
            .await;
        let bounded_router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(bounded_server.url()),
            ),
        );
        let bounded_application = app(configured_state_with_router_and_identity(
            bounded_router,
            budgeted_identity(10.0, false),
        ));
        for _ in 0..2 {
            let response = bounded_application
                .clone()
                .oneshot(request_for("openrouter/x-ai/grok-4.7"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        bounded_upstream.assert_async().await;

        let mut unpriced_server = mockito::Server::new_async().await;
        let unpriced_upstream = unpriced_server
            .mock("POST", "/chat/completions")
            .with_body(response_body)
            .expect(1)
            .create_async()
            .await;
        let unpriced_router = Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local",
                unpriced_server.url(),
                None,
            )),
        );
        let unpriced_application = app(configured_state_with_router_and_identity(
            unpriced_router,
            budgeted_identity(1.0, true),
        ));
        let first = unpriced_application
            .clone()
            .oneshot(request_for("local/test"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = unpriced_application
            .oneshot(request_for("local/test"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        unpriced_upstream.assert_async().await;
    }

    #[tokio::test]
    async fn incomplete_unary_and_partial_stream_usage_retain_the_full_quote() {
        let request_for = |stream: bool| {
            Request::builder()
                .method("POST")
                .uri(if stream {
                    "/v1/chat/stream"
                } else {
                    "/v1/chat"
                })
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-key")
                .body(Body::from(
                    json!({
                        "model": "openrouter/x-ai/grok-4.7",
                        "messages": [{"role": "user", "content": "hi"}],
                        "stream": stream
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let mut unary_server = mockito::Server::new_async().await;
        let unary_upstream = unary_server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "response",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 7}
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let unary_router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(unary_server.url()),
            ),
        );
        let unary_application = app(configured_state_with_router_and_identity(
            unary_router,
            budgeted_identity(10.0, false),
        ));
        assert_eq!(
            unary_application
                .clone()
                .oneshot(request_for(false))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            unary_application
                .oneshot(request_for(false))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        unary_upstream.assert_async().await;

        let mut stream_server = mockito::Server::new_async().await;
        let partial_stream = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"r","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}),
            json!({"id":"r","choices":[{"index":0,"delta":{},"finish_reason":null}],"usage":{"prompt_tokens":7}}),
            json!({"id":"r","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        );
        let stream_upstream = stream_server
            .mock("POST", "/chat/completions")
            .with_header("content-type", "text/event-stream")
            .with_body(partial_stream)
            .expect(1)
            .create_async()
            .await;
        let stream_router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(stream_server.url()),
            ),
        );
        let stream_application = app(configured_state_with_router_and_identity(
            stream_router,
            budgeted_identity(10.0, false),
        ));
        let stream_response = stream_application
            .clone()
            .oneshot(request_for(true))
            .await
            .unwrap();
        assert_eq!(stream_response.status(), StatusCode::OK);
        let _ = to_bytes(stream_response.into_body(), 100_000)
            .await
            .unwrap();
        assert_eq!(
            stream_application
                .oneshot(request_for(false))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        stream_upstream.assert_async().await;
    }

    #[tokio::test]
    async fn later_chat_and_gemini_activity_revokes_early_terminal_usage_authority() {
        let chat_unsafe = vec![
            vec![
                json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
                json!({"choices":[{"index":0,"delta":{"content":"later"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ],
            vec![
                json!({"choices":[
                    {"index":0,"delta":{},"finish_reason":"stop"},
                    {"index":1,"delta":{},"finish_reason":null}
                ],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
                json!({"choices":[{"index":1,"delta":{"content":"later"},"finish_reason":null}]}),
                json!({"choices":[{"index":1,"delta":{},"finish_reason":"stop"}]}),
            ],
            vec![
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
                json!({"choices":[{"index":1,"delta":{"content":"new"},"finish_reason":null}]}),
                json!({"choices":[{"index":1,"delta":{},"finish_reason":"stop"}]}),
            ],
        ];
        for sequence in chat_unsafe {
            assert_chat_stream_budget_finality(sequence, StatusCode::TOO_MANY_REQUESTS).await;
        }
        assert_chat_stream_budget_finality(
            vec![
                json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
            ],
            StatusCode::OK,
        )
        .await;
        assert_chat_stream_budget_finality(
            vec![
                json!({
                    "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}],
                    "usage":{"cost":1.0}
                }),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
            ],
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
        assert_chat_stream_budget_finality(
            vec![
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"cost":0.25}}),
                json!({"choices":[],"usage":{"prompt_tokens":1_000_000,"completion_tokens":0}}),
            ],
            StatusCode::OK,
        )
        .await;
        assert_chat_stream_budget_finality(
            vec![
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"cost":0.25}}),
                json!({"choices":[{"index":1,"delta":{"content":"later"},"finish_reason":null}]}),
                json!({"choices":[{"index":1,"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":1_000_000,"completion_tokens":0}}),
            ],
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;

        let gemini_unsafe = vec![
            vec![
                json!({"candidates":[],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"cost":1.0}}),
                json!({"candidates":[{"index":0,"content":{"parts":[{"text":"later"}]}}]}),
                json!({"candidates":[{"index":0,"finishReason":"STOP","content":{"parts":[]}}]}),
            ],
            vec![
                json!({"candidates":[
                    {"index":0,"finishReason":"STOP","content":{"parts":[]}},
                    {"index":1,"content":{"parts":[]}}
                ],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"cost":1.0}}),
                json!({"candidates":[{"index":1,"content":{"parts":[{"text":"later"}]}}]}),
                json!({"candidates":[{"index":1,"finishReason":"STOP","content":{"parts":[]}}]}),
            ],
            vec![
                json!({"candidates":[{"index":0,"finishReason":"STOP","content":{"parts":[]}}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"cost":1.0}}),
                json!({"candidates":[{"index":1,"content":{"parts":[{"text":"new"}]}}]}),
                json!({"candidates":[{"index":1,"finishReason":"STOP","content":{"parts":[]}}]}),
            ],
        ];
        for sequence in gemini_unsafe {
            assert_gemini_stream_budget_finality(sequence, StatusCode::TOO_MANY_REQUESTS).await;
        }
        assert_gemini_stream_budget_finality(
            vec![
                json!({"candidates":[{"index":0,"content":{"parts":[{"text":"ok"}]}}]}),
                json!({"candidates":[{"index":0,"finishReason":"STOP","content":{"parts":[]}}]}),
                json!({"candidates":[],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"cost":1.0}}),
            ],
            StatusCode::OK,
        )
        .await;
        assert_gemini_stream_budget_finality(
            vec![
                json!({"candidates":[{"index":0,"finishReason":"STOP","content":{"parts":[]}}],"usageMetadata":{"cost":0.25}}),
                json!({"candidates":[],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}),
            ],
            StatusCode::OK,
        )
        .await;
        assert_gemini_stream_budget_finality(
            vec![
                json!({"candidates":[{"index":0,"finishReason":"STOP","content":{"parts":[]}}],"usageMetadata":{"cost":0.25}}),
                json!({"candidates":[{"index":1,"content":{"parts":[{"text":"later"}]}}]}),
                json!({"candidates":[{"index":1,"finishReason":"STOP","content":{"parts":[]}}]}),
                json!({"candidates":[],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}),
            ],
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
    }

    #[tokio::test]
    async fn caller_supplied_budget_policy_fields_cannot_unfreeze_a_key() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/responses")
            .expect(0)
            .create_async()
            .await;
        let router = Router::new().register(
            "openai",
            Box::new(
                crate::providers::openai::OpenAi::new("test-key".into())
                    .with_base_url(server.url()),
            ),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(
                json!({
                    "model": "openai/gpt-5.6-luna",
                    "messages": [{"role": "user", "content": "hi"}],
                    "provider_config": {
                        "tenant": "forged",
                        "budget_usd": 1000000,
                        "budget_allow_unpriced": true
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app(configured_state_with_router_and_identity(
            router,
            budgeted_identity(0.0, false),
        ))
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        upstream.assert_async().await;

        let mut free_server = mockito::Server::new_async().await;
        let free_upstream = free_server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let free_router = Router::new().register(
            "openrouter",
            Box::new(
                crate::providers::openrouter::OpenRouter::new("test-key".into())
                    .with_base_url(free_server.url()),
            ),
        );
        let free_request = Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(
                json!({
                    "model": "openrouter/openrouter/free",
                    "messages": [{"role": "user", "content": "hi"}]
                })
                .to_string(),
            ))
            .unwrap();
        let free_response = app(configured_state_with_router_and_identity(
            free_router,
            budgeted_identity(0.0, false),
        ))
        .oneshot(free_request)
        .await
        .unwrap();
        assert_eq!(free_response.status(), StatusCode::TOO_MANY_REQUESTS);
        free_upstream.assert_async().await;
    }

    #[tokio::test]
    async fn upstream_body_cannot_forge_an_unpriceable_policy_refusal() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(400)
            .with_body("attempt cannot be admitted under the active policy")
            .expect(1)
            .create_async()
            .await;
        let state = configured_state(&server.url());
        let response = app(state)
            .oneshot(refusal_request("/v1/chat", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
                idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                    Duration::from_secs(30),
                ),
                idempotency_ttl_secs: 30,
                prequeue_backpressure: Backpressure::new(
                    config.max_concurrency_per_provider,
                    config.max_wait,
                ),
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

    #[tokio::test]
    async fn gateway_keeps_responses_history_stateless_after_native_overrides() {
        let captured_requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let captured_upstream_requests = captured_requests.clone();
        let mut upstream_server = mockito::Server::new_async().await;
        let upstream = upstream_server
            .mock("POST", "/responses")
            .match_header("authorization", "Bearer upstream-test-key")
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let native_request: Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                captured_upstream_requests
                    .lock()
                    .unwrap()
                    .push(native_request.clone());
                let native_response = json!({
                    "id": "resp_test",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "explicit history only"}]
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                });
                if native_request["stream"] == true {
                    format!(
                        "data: {}\n\ndata: {}\n\n",
                        json!({
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "content_index": 0,
                            "item_id": "msg_test",
                            "delta": "explicit history only"
                        }),
                        json!({"type": "response.completed", "response": native_response})
                    )
                    .into_bytes()
                } else {
                    native_response.to_string().into_bytes()
                }
            })
            .expect(12)
            .create_async()
            .await;
        let router = Router::new().register(
            "openai",
            Box::new(
                crate::providers::openai::OpenAi::new("upstream-test-key".into())
                    .with_base_url(upstream_server.url()),
            ),
        );
        let state = configured_state_with_router(router);
        let receipt_directory = tempfile::tempdir().unwrap();
        let receipts = Arc::new(crate::proxy::wire::Receipts::new(
            receipt_directory.path().to_owned(),
        ));
        for (path, streaming) in [
            ("/v1/chat", false),
            ("/v1/chat/stream", true),
            ("/v1/chat/completions", false),
            ("/v1/chat/completions", true),
            ("/v1/messages", false),
            ("/v1/messages", true),
        ] {
            for conversation in [json!("conv_other"), json!({"id": "conv_other"})] {
                let overrides = json!({
                    "conversation": conversation,
                    "previous_response_id": "resp_other",
                    "store": true
                });
                let mut request = json!({
                    "model": "openai/gpt-6-astra",
                    "messages": [{"role": "user", "content": "explicit history"}],
                    "stream": streaming
                });
                if path == "/v1/chat" || path == "/v1/chat/stream" {
                    request["config"] = json!({"max_tokens": 32});
                    request["provider_config"] = json!({"x-openai": overrides});
                } else {
                    request["max_tokens"] = json!(32);
                    request["x-openai"] = overrides;
                }
                let response = app(state.clone())
                    .layer(Extension(receipts.clone()))
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(path)
                            .header("content-type", "application/json")
                            .header("authorization", "Bearer test-key")
                            .body(Body::from(request.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{path}");
                let response_body = to_bytes(response.into_body(), 16_384).await.unwrap();
                let response_body = String::from_utf8(response_body.to_vec()).unwrap();
                assert!(response_body.contains("explicit history only"));
                assert!(!response_body.contains("event: error"));
            }
        }
        upstream.assert_async().await;
        let captured_requests = captured_requests.lock().unwrap();
        assert_eq!(captured_requests.len(), 12);
        for request in captured_requests.iter() {
            assert_eq!(request["store"], false);
            assert!(request.get("previous_response_id").is_none());
            assert!(request.get("conversation").is_none());
            assert!(request["input"].to_string().contains("explicit history"));
        }
    }
}
