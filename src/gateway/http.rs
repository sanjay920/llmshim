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
use crate::router::{Router, RouterHandle};

/// The [`Dispatch`] that actually fires upstream LLM calls once the scheduler
/// admits a job — via the crate's normal `completion` / `stream` paths. Detects
/// an upstream 429 so the scheduler can penalize the provider's bucket.
pub struct RealDispatch {
    router: RouterHandle,
    logger: Option<Logger>,
    /// The back-off after an upstream 429 that names no `Retry-After`, when the
    /// file sets one; otherwise `LLMSHIM_PENALTY_SECS` (or 5 s).
    penalty: Option<std::time::Duration>,
}

impl RealDispatch {
    fn with_response_identity(mut value: Value) -> Value {
        // Distributed replay reads the persisted dispatch result, so identity
        // must be assigned before either backend stores the completion.
        if !value["created_at"].is_i64() && !value["created"].is_i64() {
            value["created_at"] = json!(chrono::Utc::now().timestamp());
        }
        if !value["id"].as_str().is_some_and(|id| !id.is_empty()) {
            value["id"] = json!(format!("chat_{}", uuid::Uuid::new_v4().simple()));
        }
        value
    }

    fn penalty(&self) -> std::time::Duration {
        self.penalty.unwrap_or_else(penalty_duration)
    }

    fn map_err(
        &self,
        err: ShimError,
        refusal: Option<crate::policy::AttemptPolicyRefusal>,
    ) -> DispatchError {
        let penalty = self.penalty();
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
                    retry_after.unwrap_or(penalty).as_millis()
                ),
                retry_after: Some(retry_after.unwrap_or(penalty)),
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
            // A replica whose router does not know the provider (a roll-out in
            // progress, or a removal, 0022): a local condition, not an upstream
            // failure. It happens before any attempt exists, so nothing was sent,
            // no spend was reserved and the breaker never sees it.
            #[cfg(feature = "serve")]
            ShimError::UnknownProvider(_) => DispatchError {
                message: format!("llmshim-provider-unavailable:{}", penalty.as_millis()),
                retry_after: Some(penalty),
            },
            other => DispatchError::new(other.to_string()),
        }
    }
}

#[async_trait]
impl Dispatch for RealDispatch {
    async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
        crate::completion_with_logger(
            self.router.current().as_ref(),
            &payload,
            self.logger.as_ref(),
        )
        .await
        .map(Self::with_response_identity)
        .map_err(|error| self.map_err(error, None))
    }

    async fn dispatch_with_policy(
        &self,
        _provider: &str,
        payload: Value,
        policy_context: crate::policy::DispatchPolicyContext,
    ) -> Result<Value, DispatchError> {
        let router = policy_context
            .router_snapshot()
            .cloned()
            .unwrap_or_else(|| self.router.current());
        // An embeddings call (note 0035) is told apart by the trusted context, never by the body.
        if policy_context.operation() == crate::policy::DispatchOperation::Embeddings {
            let result =
                crate::embeddings_value_with_policy(router.as_ref(), &payload, &policy_context)
                    .await;
            let refusal = policy_context.take_last_refusal();
            return result.map_err(|error| self.map_err(error, refusal));
        }
        let result = crate::completion_with_logger_and_policy(
            router.as_ref(),
            &payload,
            self.logger.as_ref(),
            &policy_context,
        )
        .await;
        let refusal = policy_context.take_last_refusal();
        result
            .map(Self::with_response_identity)
            .map_err(|error| self.map_err(error, refusal))
    }

    async fn dispatch_stream(
        &self,
        _provider: &str,
        payload: Value,
    ) -> Result<ChunkStream, DispatchError> {
        let upstream = crate::stream_with_logger(
            self.router.current().as_ref(),
            &payload,
            self.logger.as_ref(),
        )
        .await
        .map_err(|error| self.map_err(error, None))?;
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
        let router = policy_context
            .router_snapshot()
            .cloned()
            .unwrap_or_else(|| self.router.current());
        let opened = crate::stream_with_logger_and_policy(
            router.as_ref(),
            &payload,
            self.logger.as_ref(),
            &policy_context,
        )
        .await;
        let refusal = policy_context.take_last_refusal();
        let upstream = opened.map_err(|error| self.map_err(error, refusal))?;
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

#[cfg(feature = "redis-coordination")]
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

enum PreparedGatewaySubmission {
    Local {
        request: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
    },
    /// Admitted locally against a lease while Redis is out (note 0027).
    #[cfg(feature = "redis-coordination")]
    Outage {
        request: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
        guard: crate::gateway::outage::InFlight,
    },
    #[cfg(feature = "redis-coordination")]
    Distributed(crate::gateway::distributed::PreparedSubmission),
}

struct SubmittedValue {
    value: Value,
    #[cfg(feature = "redis-coordination")]
    lifecycle_reference: Option<crate::gateway::distributed::LifecycleReference>,
}

/// Bounded local admission while Redis is out (design note 0027): the manager
/// that holds the leases, and a scheduler and attempt coordinator of its own,
/// limited to this replica's share.
#[cfg(feature = "redis-coordination")]
struct OutageSide {
    manager: Arc<crate::gateway::outage::OutageManager>,
    scheduler: Arc<Scheduler>,
    coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
    rate_fraction: f64,
}

/// The `[fleet.outage_admission]` settings.
#[cfg(feature = "redis-coordination")]
#[derive(Clone, Debug)]
pub(crate) struct OutageParts {
    pub spend_lease_fraction: f64,
    pub rate_fraction: f64,
    pub max_concurrency: usize,
    pub recover_after_secs: u64,
}

/// The local ledger is where recovery reads what a lease spent.
#[cfg(feature = "redis-coordination")]
#[async_trait::async_trait]
impl crate::gateway::outage::SpendSource for crate::gateway::attempt::AttemptCoordinator {
    async fn spent_nanos(&self, tenant_key: &str, epoch: u32, window_index: u64) -> Option<u64> {
        self.local_budget_total(
            &crate::gateway::attempt::outage_tenant_key(tenant_key, epoch, window_index),
            crate::gateway::attempt::OUTAGE_LOCAL_WINDOW_SECS,
        )
        .await
    }
}

/// Shared state for the gateway HTTP handlers.
pub struct GatewayState {
    router: RouterHandle,
    backend: Backend,
    keystore: crate::gateway::auth::KeyStore,
    attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
    idempotency: crate::gateway::idempotency::IdempotencyCache,
    #[cfg_attr(not(feature = "redis-coordination"), allow(dead_code))]
    idempotency_ttl_secs: u64,
    prequeue_backpressure: Backpressure,
    /// `Retry-After` suggested when a job's queue wait times out.
    overloaded_retry_after: Duration,
    #[cfg(feature = "redis-coordination")]
    outage: Option<OutageSide>,
}

/// Everything the single-instance gateway state is built from. Grouping these
/// lets a caller supply them from a configuration file instead of the
/// environment, and makes the set of inputs explicit.
pub(crate) struct LocalParts {
    pub config: GatewayConfig,
    pub breaker: Arc<crate::breaker::ProviderBreaker>,
    /// Admission limiter for the scheduler's dispatch loop.
    pub limiter: Arc<dyn crate::proxy::ratelimit::RateLimiter>,
    /// Limits for the per-attempt coordinator (retries, repairs, fallbacks).
    pub rate_limits: crate::proxy::ratelimit::RateLimitConfig,
    pub keystore: crate::gateway::auth::KeyStore,
    pub idempotency_ttl_secs: u64,
    /// Finished attempts the spend ledger keeps for late settlement.
    pub max_retained_accounting_attempts: usize,
    /// Receives one record per provider attempt that ends.
    pub usage: Option<crate::gateway::usage::UsageSetup>,
    /// Follow a tenant's limit when a later request carries a different one (a
    /// reloaded configuration re-limited it). The gateway's own construction
    /// leaves this off, so a tenant's bucket keeps the limit it began with.
    pub retune_tenant_limits: bool,
    /// Back-off after an upstream 429 (`limits.penalty_secs`); `None` keeps the
    /// environment's `LLMSHIM_PENALTY_SECS` (or 5 s).
    pub penalty: Option<std::time::Duration>,
}

/// Everything the fleet (Redis-backed) gateway state is built from.
#[cfg(feature = "redis-coordination")]
pub(crate) struct DistributedParts {
    pub config: GatewayConfig,
    pub tuning: crate::gateway::distributed::Tuning,
    pub breaker: Arc<crate::breaker::ProviderBreaker>,
    pub limiter: Arc<dyn crate::proxy::ratelimit::RateLimiter>,
    pub rate_limits: crate::proxy::ratelimit::RateLimitConfig,
    pub keystore: crate::gateway::auth::KeyStore,
    pub idempotency_ttl_secs: u64,
    pub max_retained_accounting_attempts: usize,
    pub usage: Option<crate::gateway::usage::UsageSetup>,
    /// See [`LocalParts::penalty`].
    pub penalty: Option<std::time::Duration>,
    /// Bounded admission while Redis is out; `None` keeps today's behaviour.
    pub outage: Option<OutageParts>,
}

impl LocalParts {
    /// The environment-driven parts, read in the order `from_env` always read
    /// them (startup messages keep their order).
    fn from_env() -> Self {
        let config = GatewayConfig::from_env();
        let breaker = crate::proxy::health::build_breaker();
        let limiter = build_limiter();
        let rate_limits = crate::proxy::ratelimit::RateLimitConfig::from_env();
        let keystore = crate::gateway::auth::KeyStore::from_env();
        Self {
            config,
            breaker,
            limiter,
            rate_limits,
            keystore,
            idempotency_ttl_secs: idem_ttl_secs(),
            max_retained_accounting_attempts:
                crate::gateway::attempt::retained_accounting_attempt_limit_from(&|key| {
                    std::env::var(key).ok()
                }),
            usage: None,
            retune_tenant_limits: false,
            penalty: None,
        }
    }
}

impl GatewayState {
    /// Build in-memory (single-instance) gateway state, reading the rate limiter
    /// and scheduler tuning from the environment (same rate-limit knobs as the
    /// proxy, plus `LLMSHIM_GATEWAY_*`).
    pub fn from_env(router: Router, logger: Option<Logger>) -> Arc<Self> {
        Self::from_parts(router, logger, LocalParts::from_env())
    }

    /// Build in-memory (single-instance) gateway state from explicit parts.
    /// The parts themselves are read from nothing; a few process-wide settings
    /// are still read later (retained accounting attempts, the penalty after a
    /// 429, the shared HTTP client), as the 0002 design note lists.
    /// [`Self::from_env`] is this function applied to [`LocalParts::from_env`].
    pub(crate) fn from_parts(
        router: Router,
        logger: Option<Logger>,
        parts: LocalParts,
    ) -> Arc<Self> {
        let LocalParts {
            config,
            breaker,
            limiter,
            rate_limits,
            keystore,
            idempotency_ttl_secs,
            max_retained_accounting_attempts,
            usage,
            retune_tenant_limits,
            penalty,
        } = parts;
        // Same fleet-wide provider health the proxy uses.
        let router = RouterHandle::new(router.with_breaker(breaker));
        let dispatch = Arc::new(RealDispatch {
            router: router.clone(),
            logger,
            penalty,
        });
        let scheduler = Scheduler::new(config.clone(), limiter, dispatch);
        let attempt_coordinator = crate::gateway::attempt::AttemptCoordinator::local_with_retention(
            rate_limits,
            config.max_concurrency_per_provider,
            config.max_wait,
            max_retained_accounting_attempts,
            retune_tenant_limits,
        );
        if let Some(setup) = usage {
            attempt_coordinator.set_usage_sink(setup);
        }
        if let Some(penalty) = penalty {
            attempt_coordinator.set_penalty(penalty);
        }
        Arc::new(Self {
            router,
            backend: Backend::Local(scheduler),
            keystore,
            attempt_coordinator,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idempotency_ttl_secs),
            ),
            idempotency_ttl_secs,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
            overloaded_retry_after: config.overloaded_retry_after,
            #[cfg(feature = "redis-coordination")]
            outage: None,
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
        let tuning = crate::gateway::distributed::Tuning::from_env(&config);
        // Same fleet-wide provider health the proxy uses.
        let breaker = crate::proxy::health::build_breaker();
        let rate_limits = crate::proxy::ratelimit::RateLimitConfig::from_env();
        let limiter = build_limiter();
        let keystore = crate::gateway::auth::KeyStore::from_env();
        Self::distributed_from_parts(
            router,
            logger,
            redis_url,
            DistributedParts {
                config,
                tuning,
                breaker,
                limiter,
                rate_limits,
                keystore,
                idempotency_ttl_secs: idem_ttl_secs(),
                max_retained_accounting_attempts:
                    crate::gateway::attempt::retained_accounting_attempt_limit_from(&|key| {
                        std::env::var(key).ok()
                    }),
                usage: None,
                penalty: None,
                outage: None,
            },
        )
        .await
    }

    /// Build fleet gateway state from explicit parts, reading nothing from the
    /// environment. [`Self::distributed_from_env`] is this function applied to
    /// environment-derived parts (the caller-key file is now read before the
    /// Redis connection rather than after it).
    #[cfg(feature = "redis-coordination")]
    pub(crate) async fn distributed_from_parts(
        router: Router,
        logger: Option<Logger>,
        redis_url: &str,
        parts: DistributedParts,
    ) -> Result<Arc<Self>, String> {
        let state = Self::distributed_unstarted(router, logger, redis_url, parts).await?;
        state.start_workers();
        Ok(state)
    }

    /// Everything [`Self::distributed_from_parts`] does except start the workers
    /// that lease jobs from the shared queue. A caller that can still fail after
    /// this (`serve` binds its listeners next) must not have workers running, or
    /// a replica that exits at once leaves leases on peers' jobs until they
    /// expire and spends one delivery attempt from each.
    #[cfg(feature = "redis-coordination")]
    pub(crate) async fn distributed_unstarted(
        router: Router,
        logger: Option<Logger>,
        redis_url: &str,
        parts: DistributedParts,
    ) -> Result<Arc<Self>, String> {
        let DistributedParts {
            config,
            tuning,
            breaker,
            limiter,
            rate_limits,
            keystore,
            idempotency_ttl_secs,
            max_retained_accounting_attempts,
            usage,
            penalty,
            outage,
        } = parts;
        let router = RouterHandle::new(router.with_breaker(breaker));
        let dispatch = Arc::new(RealDispatch {
            router: router.clone(),
            logger,
            penalty,
        });
        let dispatch_for_outage = dispatch.clone();
        let outage_usage = usage.clone();
        let outage_rate_limits = rate_limits.clone();
        let attempt_coordinator =
            crate::gateway::attempt::AttemptCoordinator::redis_with_retention(
                redis_url,
                rate_limits,
                config.max_concurrency_per_provider,
                config.max_wait,
                max_retained_accounting_attempts,
            )
            .map_err(|error| error.to_string())?;
        if let Some(setup) = usage {
            attempt_coordinator.set_usage_sink(setup);
        }
        if let Some(penalty) = penalty {
            attempt_coordinator.set_penalty(penalty);
        }
        let gateway = crate::gateway::distributed::DistributedGateway::connect_with_tuning(
            redis_url,
            dispatch,
            limiter,
            config.clone(),
            attempt_coordinator.clone(),
            tuning,
        )
        .await
        .map_err(|e| e.to_string())?;
        let outage = match outage {
            Some(parts) => Some(Self::build_outage_side(
                redis_url,
                &parts,
                &config,
                outage_rate_limits,
                max_retained_accounting_attempts,
                outage_usage,
                penalty,
                dispatch_for_outage,
            )?),
            None => None,
        };
        Ok(Arc::new(Self {
            router,
            backend: Backend::Distributed(gateway),
            keystore,
            attempt_coordinator,
            idempotency: crate::gateway::idempotency::IdempotencyCache::new(
                std::time::Duration::from_secs(idempotency_ttl_secs),
            ),
            idempotency_ttl_secs,
            prequeue_backpressure: Backpressure::new(
                config.max_concurrency_per_provider,
                config.max_wait,
            ),
            overloaded_retry_after: config.overloaded_retry_after,
            #[cfg(feature = "redis-coordination")]
            outage,
        }))
    }

    /// Start the workers that serve every configured provider's shared queue.
    /// Call once; a local (single-instance) state has none.
    #[cfg(feature = "redis-coordination")]
    pub(crate) fn start_workers(&self) {
        if let Backend::Distributed(gateway) = &self.backend {
            gateway.start_managed_workers(|| {
                self.router
                    .current()
                    .provider_keys()
                    .into_iter()
                    .map(String::from)
                    .collect()
            });
        }
    }

    /// Make the fleet's workers follow the router's provider set after a reload
    /// (0022). A no-op for a single instance, whose lanes are made on demand.
    #[cfg(feature = "serve")]
    pub(crate) fn sync_fleet_workers(&self) {
        #[cfg(feature = "redis-coordination")]
        if let Backend::Distributed(gateway) = &self.backend {
            let providers: Vec<String> = self
                .router
                .current()
                .provider_keys()
                .into_iter()
                .map(String::from)
                .collect();
            gateway.sync_workers(providers);
        }
    }

    /// Pin the router a request was admitted on, where the job will run on this
    /// replica. A fleet job is run by whichever replica leases it, on that
    /// replica's router (0022), so there the snapshot would only be held for
    /// nothing.
    /// Retune every holder of the provider limits (0022): the scheduler's limiter
    /// (or the fleet's), and the coordinator that admits retries, repairs and
    /// fallbacks, whose limiter (local or Redis) is the other.
    #[cfg(feature = "serve")]
    pub(crate) fn apply_limits(&self, config: crate::proxy::ratelimit::RateLimitConfig) {
        match &self.backend {
            Backend::Local(scheduler) => scheduler.retune_limits(config.clone()),
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.retune_limits(config.clone()),
        }
        // The outage path's own limits are this replica's share of the new ones.
        #[cfg(feature = "redis-coordination")]
        if let Some(outage) = &self.outage {
            let share = config.scaled(outage.rate_fraction);
            outage.scheduler.retune_limits(share.clone());
            outage.coordinator.retune_limits(share);
        }
        self.attempt_coordinator.retune_limits(config);
    }

    /// The handle a reload replaces the router through (0022).
    #[cfg(feature = "serve")]
    pub(crate) fn router_handle(&self) -> RouterHandle {
        self.router.clone()
    }

    fn pin_router(
        &self,
        scope: &mut crate::gateway::attempt::TrustedPolicyScope,
        snapshot: Arc<Router>,
    ) {
        if matches!(self.backend, Backend::Local(_)) {
            scope.set_router(snapshot);
        }
    }

    async fn trusted_policy_scope(
        &self,
        identity: &crate::gateway::auth::Identity,
    ) -> Result<crate::gateway::attempt::TrustedPolicyScope, GatewayError> {
        let policy_scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(identity);
        #[cfg(feature = "redis-coordination")]
        let mut policy_scope = policy_scope;
        #[cfg(feature = "redis-coordination")]
        // The legacy spend floor is Redis state: during an outage the lease, taken
        // with it counted, stands in for it.
        if identity.budget_usd.is_some() && !self.outage_active() {
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

    /// The local half of bounded outage admission: a manager that holds the
    /// leases, a scheduler and an attempt coordinator of this replica's share,
    /// and the task that ticks the manager.
    #[cfg(feature = "redis-coordination")]
    #[allow(clippy::too_many_arguments)]
    fn build_outage_side(
        redis_url: &str,
        parts: &OutageParts,
        config: &GatewayConfig,
        rate_limits: crate::proxy::ratelimit::RateLimitConfig,
        max_retained_accounting_attempts: usize,
        usage: Option<crate::gateway::usage::UsageSetup>,
        penalty: Option<std::time::Duration>,
        dispatch: Arc<dyn Dispatch>,
    ) -> Result<OutageSide, String> {
        let scaled = rate_limits.scaled(parts.rate_fraction);
        let mut local_config = config.clone();
        local_config.max_concurrency_per_provider = parts.max_concurrency;
        // A short queue: the mode is not a queue, it smooths a burst.
        local_config.max_queue_depth = parts.max_concurrency;
        local_config.max_wait = local_config.max_wait.min(Duration::from_secs(1));
        let limiter: Arc<dyn crate::proxy::ratelimit::RateLimiter> = Arc::new(
            crate::proxy::ratelimit::InMemoryRateLimiter::new(scaled.clone()),
        );
        let scheduler = Scheduler::new(local_config.clone(), limiter, dispatch);
        let coordinator = crate::gateway::attempt::AttemptCoordinator::local_with_retention(
            scaled,
            parts.max_concurrency,
            local_config.max_wait,
            max_retained_accounting_attempts,
            false,
        );
        if let Some(setup) = usage {
            coordinator.set_outage_usage_sink(setup);
        }
        if let Some(penalty) = penalty {
            coordinator.set_penalty(penalty);
        }
        let ledger = crate::gateway::lease::LeaseLedger::new(
            redis_url,
            crate::gateway::attempt::LATE_SETTLEMENT_RETENTION_SECS,
            crate::gateway::attempt::REDIS_ACCOUNTING_INDEX_KEY.to_owned(),
            max_retained_accounting_attempts,
        )
        .map_err(|error| error.to_string())?;
        let replica = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let manager = Arc::new(crate::gateway::outage::OutageManager::new(
            ledger,
            coordinator.clone(),
            replica,
            parts.spend_lease_fraction,
            parts.recover_after_secs.saturating_mul(1000),
        ));
        // The task ends when the gateway state (and with it the manager) is gone.
        let ticker = Arc::downgrade(&manager);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                let Some(manager) = ticker.upgrade() else {
                    break;
                };
                manager.tick(epoch_ms()).await;
            }
        });
        Ok(OutageSide {
            manager,
            scheduler,
            coordinator,
            rate_fraction: parts.rate_fraction,
        })
    }

    /// Admit a request locally against this replica's lease, or refuse it. Only
    /// reached in outage state.
    #[cfg(feature = "redis-coordination")]
    fn prepare_outage(
        &self,
        outage: &OutageSide,
        req: GatewayRequest,
        policy_scope: &crate::gateway::attempt::TrustedPolicyScope,
    ) -> Result<PreparedGatewaySubmission, GatewayError> {
        let refuse = || GatewayError::Overloaded(self.overloaded_retry_after);
        let (tenant, window_secs, _) = policy_scope.outage_budget().ok_or_else(refuse)?;
        let tenant = tenant.as_str();
        let (admission, guard) = outage
            .manager
            .admit(epoch_ms(), tenant, window_secs)
            .map_err(|_| refuse())?;
        let view = policy_scope
            .outage_view(
                admission.held_nanos,
                admission.epoch,
                admission.window_index,
                outage.rate_fraction,
            )
            .ok_or_else(refuse)?;
        Ok(PreparedGatewaySubmission::Outage {
            request: req,
            policy_scope: view,
            guard,
        })
    }

    fn prepare_submission(
        &self,
        req: GatewayRequest,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
        stream: bool,
    ) -> Result<PreparedGatewaySubmission, GatewayError> {
        #[cfg(not(feature = "redis-coordination"))]
        let _ = stream;
        #[cfg(feature = "redis-coordination")]
        if let Some(outage) = &self.outage {
            if outage.manager.routes_locally() {
                return self.prepare_outage(outage, req, &policy_scope);
            }
            // Healthy: note the budgeted tenant so a lease is taken for it.
            if let Some((tenant, window_secs, limit)) = policy_scope.outage_budget() {
                outage.manager.register(&tenant, window_secs, limit);
            }
        }
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
            PreparedGatewaySubmission::Outage {
                request,
                policy_scope,
                guard,
            } => {
                let outage = self.outage.as_ref().expect("outage admission was prepared");
                drop(prequeue_permit);
                let policy_context = outage.coordinator.context(policy_scope);
                let value = match logical_deadline {
                    Some(deadline) => {
                        outage
                            .scheduler
                            .submit_with_policy_deadline(request, policy_context, deadline)
                            .await
                    }
                    None => {
                        outage
                            .scheduler
                            .submit_with_policy(request, policy_context)
                            .await
                    }
                };
                drop(guard);
                Ok(SubmittedValue {
                    value: value?,
                    lifecycle_reference: None,
                })
            }
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
            PreparedGatewaySubmission::Outage {
                request,
                policy_scope,
                guard,
            } => {
                let outage = self.outage.as_ref().expect("outage admission was prepared");
                drop(prequeue_permit);
                let policy_context = outage.coordinator.context(policy_scope);
                let mut inner = match logical_deadline {
                    Some(deadline) => {
                        outage
                            .scheduler
                            .submit_stream_with_policy_deadline(request, policy_context, deadline)
                            .await
                    }
                    None => {
                        outage
                            .scheduler
                            .submit_stream_with_policy(request, policy_context)
                            .await
                    }
                }?;
                // The guard lives as long as the stream, so recovery waits for it.
                let (tx, rx) = mpsc::channel(32);
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            chunk = inner.recv() => match chunk {
                                Some(chunk) => {
                                    if tx.send(chunk).await.is_err() {
                                        break;
                                    }
                                }
                                None => break,
                            },
                            // The client went away: stop relaying at once, so the
                            // guard is not held for a stalled upstream.
                            () = tx.closed() => break,
                        }
                    }
                    drop(guard);
                });
                Ok(rx)
            }
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
                let router = self.router.current();
                let providers: Vec<String> = router
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
        #[cfg(feature = "redis-coordination")]
        if let Some(outage) = &self.outage {
            metrics::gauge_set(
                metrics::OUTAGE_ADMISSION_ACTIVE,
                &[],
                i64::from(outage.manager.routes_locally()),
            );
            metrics::gauge_set(
                metrics::OUTAGE_LEASE_NANOS,
                &[],
                i64::try_from(outage.manager.held_nanos()).unwrap_or(i64::MAX),
            );
        }
    }

    /// Cached response for a credential-, route-, and request-bound key.
    async fn idem_lookup(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
    ) -> crate::gateway::idempotency::IdempotencyLookup {
        // Shared idempotency lives in Redis; during an outage a replica keeps its
        // own in memory (a retry that reaches another replica can run twice).
        #[cfg(feature = "redis-coordination")]
        if self.outage_active() {
            return self.idempotency.lookup(context);
        }
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
        #[cfg(feature = "redis-coordination")]
        if self.outage_active() {
            self.idempotency.store(context, value.clone());
            return;
        }
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

    #[cfg(feature = "redis-coordination")]
    fn outage_active(&self) -> bool {
        self.outage
            .as_ref()
            .is_some_and(|outage| outage.manager.routes_locally())
    }

    /// Readiness: in-memory mode is always ready; distributed mode requires
    /// Redis to answer a PING.
    async fn ready(&self) -> bool {
        // With outage admission on, a replica serving from its lease stays in the
        // Service (note 0027); without a lease it is as unready as ever.
        #[cfg(feature = "redis-coordination")]
        if let Some(outage) = &self.outage {
            if outage.manager.routes_locally() {
                return outage.manager.held_nanos() > 0;
            }
        }
        match &self.backend {
            Backend::Local(_) => true,
            #[cfg(feature = "redis-coordination")]
            Backend::Distributed(gateway) => gateway.ping().await,
        }
    }

    /// Introspection snapshot for the admin stats endpoint.
    async fn stats(&self) -> Value {
        let router = self.router.current();
        let providers: Vec<String> = router
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
        #[cfg_attr(not(feature = "redis-coordination"), allow(unused_mut))]
        let mut stats = json!({
            "mode": mode,
            "auth_enforced": self.keystore.is_enforced(),
            "providers": providers,
            "lanes": lanes,
        });
        // Present only when the file has deployments: each one's targets and their circuits.
        let deployments = self.router.current().deployment_overview();
        if !deployments.is_empty() {
            stats["deployments"] = json!(deployments);
        }
        // Present only with `[fleet.outage_admission]`; never a tenant's name.
        #[cfg(feature = "redis-coordination")]
        if let Some(outage) = &self.outage {
            stats["outage_admission"] = json!({
                "active": outage.manager.routes_locally(),
                "lease_nanos": outage.manager.held_nanos(),
            });
        }
        stats
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
#[cfg(test)]
fn build_request(
    state: &GatewayState,
    headers: &HeaderMap,
    req: &ChatRequest,
    preselected: Option<Arc<Router>>,
) -> Result<
    (
        String,
        String,
        GatewayRequest,
        crate::gateway::auth::IdentifiedCaller,
        Arc<Router>,
    ),
    ApiError,
> {
    build_request_for(state, headers, req, preselected, None)
}

/// [`build_request`], with the request prepared for `forced` (a concrete target) instead of the
/// model the caller sent: a retry of a deployment request after its first target failed. The
/// caller's own model string still names the alias for the allowlist, the usage record and the
/// metrics (note 0031).
fn build_request_for(
    state: &GatewayState,
    headers: &HeaderMap,
    req: &ChatRequest,
    preselected: Option<Arc<Router>>,
    forced: Option<&str>,
) -> Result<
    (
        String,
        String,
        GatewayRequest,
        crate::gateway::auth::IdentifiedCaller,
        Arc<Router>,
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
    // One snapshot for the whole request (0022): the provider is resolved against it
    // here and the dispatch that runs the job uses the same one.
    let snapshot = preselected.unwrap_or_else(|| state.router.current());
    let prepared_request = match forced {
        Some(target) => {
            // The envelope was validated by the first attempt; only the model differs.
            let mut value = crate::proxy::convert::request_to_value(req);
            value["model"] = json!(target);
            crate::proxy::convert::prepare_engine_request(&snapshot, value)?
        }
        None => crate::proxy::convert::prepare_request(&snapshot, req)?,
    };
    let identity = &identified_caller.identity;
    if identity.allowed_models.is_some() {
        // The routing id of what the request addressed, once routes and aliases are
        // resolved: what the key's list is matched against (note 0028).
        // Matched against the model the PREPARED request carries: a deployment was resolved to one
        // target by `expand_route`, and resolving the name again here would pick a second time and
        // could disagree with what is dispatched (note 0031).
        let prepared_model = prepared_request
            .payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (provider_key, model) = snapshot
            .routing_id(prepared_model)
            .map_err(ApiError::from)?;
        let alias = (req.model.starts_with("route/") || req.model.starts_with("deployment/"))
            .then_some(req.model.as_str());
        // OpenRouter's own request fields are read by the KIND, so an instance of it is checked too.
        let kind = snapshot
            .provider_kind(&provider_key)
            .unwrap_or(&provider_key)
            .to_owned();
        if !request_models_allowed(
            identity,
            &provider_key,
            &kind,
            &model,
            alias,
            &prepared_request.payload,
        ) {
            crate::gateway::metrics::incr(
                crate::gateway::metrics::REJECTED,
                &[("provider", &provider_key), ("reason", "model_not_allowed")],
            );
            return Err(model_not_allowed());
        }
    }
    // Counted once per request: a retry is counted by the failover counter instead.
    if let Some(name) = req
        .model
        .strip_prefix("deployment/")
        .filter(|_| forced.is_none())
    {
        crate::gateway::metrics::incr(
            crate::gateway::metrics::DEPLOYMENT_SELECTED,
            &[
                ("deployment", name),
                (
                    "target",
                    prepared_request
                        .payload
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
            ],
        );
    }
    // The lane and the response's provider are the admission identity (note 0032); the kind
    // stays on `target.provider_name` for what reads vendor behavior.
    let provider_name = prepared_request.target.instance.clone();
    let budget_model = prepared_request.target.model.clone();
    let permits = estimate_prepared_request_tokens(&prepared_request);
    let gw = GatewayRequest {
        provider: provider_name.clone(),
        tier: identified_caller.identity.tier,
        permits,
        payload: prepared_request.payload,
    };
    Ok((provider_name, budget_model, gw, identified_caller, snapshot))
}

/// Whether a restricted key may send this request: its routing id (or the route it
/// addressed) is listed, and so is every model of OpenRouter's own fallback list
/// (`x-openrouter.models`) when the request carries one (note 0028).
fn request_models_allowed(
    identity: &crate::gateway::auth::Identity,
    provider_key: &str,
    kind: &str,
    model: &str,
    alias: Option<&str>,
    payload: &Value,
) -> bool {
    // Only OpenRouter reads `x-openrouter`: its fallback list names more models than the
    // primary, and a preset is configured upstream and can set the model there, so a
    // restricted key may not send one.
    let fallbacks_allowed = kind != "openrouter"
        || (payload.pointer("/x-openrouter/preset").is_none()
            && match payload.pointer("/x-openrouter/models") {
                None => true,
                Some(Value::Array(models)) => models.iter().all(|entry| {
                    entry
                        .as_str()
                        .is_some_and(|entry| identity.permits_model(provider_key, entry, None))
                }),
                Some(_) => false,
            });
    identity.permits_model(provider_key, model, alias) && fallbacks_allowed
}

/// 403 for a model outside the key's `allowed_models` (note 0028). It names no
/// model and lists nothing.
fn model_not_allowed() -> ApiError {
    ApiError::from(ShimError::ProviderError {
        status: StatusCode::FORBIDDEN.as_u16(),
        body: json!({
            "error": {
                "message": "model not allowed for this key",
                "type": "permission_error",
                "param": "model",
                "code": "model_not_allowed"
            }
        })
        .to_string(),
        retry_after: None,
    })
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
        #[cfg(feature = "serve")]
        GatewayError::Upstream(message) if message.starts_with("llmshim-provider-unavailable:") => {
            ApiError::ProviderUnavailable(
                message
                    .strip_prefix("llmshim-provider-unavailable:")
                    .and_then(|milliseconds| milliseconds.parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .unwrap_or(state.overloaded_retry_after),
            )
        }
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
    admitted_router: Option<axum::Extension<AdmittedRouter>>,
    headers: HeaderMap,
    uri: Uri,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    let preselected = admitted_router.map(|axum::Extension(AdmittedRouter(router))| router);
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
        return Ok(chat_stream_inner(
            state,
            headers,
            req,
            prequeue_permit,
            logical_lifetime,
            uri.path() == "/v1/responses",
            preselected,
        )
        .await);
    }

    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // A request addressed to a deployment may be retried on another of its targets when the one it
    // was sent to fails in a way that says nothing about the request (note 0031). Each retry is a
    // new submission to the next target's lane, with its own admission, spend and usage record.
    let mut tried: Vec<String> = Vec::new();
    let mut forced: Option<String> = None;
    let mut preselected = preselected;
    let mut prequeue_permit = Some(prequeue_permit);
    let mut idempotency_context = None;
    // The failure that led to the current retry: if the retry cannot even be prepared (the key's list
    // does not allow the next target, say), the caller gets that failure, not an error about a
    // target they never chose.
    let mut previous: Option<GatewayError> = None;
    let mut pending_failover: Option<(String, String, String)> = None;
    loop {
        let (provider_name, _budget_model, gw, identified_caller, snapshot) =
            match build_request_for(
                &state,
                &headers,
                &req,
                preselected.clone(),
                forced.as_deref(),
            ) {
                Ok(built) => built,
                Err(error) => {
                    return Err(match previous.take() {
                        Some(failure) => gateway_err_to_api(&state, failure),
                        None => error,
                    })
                }
            };
        // Every attempt of one request resolves on the first attempt's router snapshot.
        preselected = Some(snapshot.clone());
        if let Some((name, from, to)) = pending_failover.take() {
            crate::gateway::metrics::incr(
                crate::gateway::metrics::DEPLOYMENT_FAILOVER,
                &[("deployment", &name), ("from", &from), ("to", &to)],
            );
        }
        let identity = &identified_caller.identity;
        let first_attempt = tried.is_empty();
        if first_attempt {
            idempotency_context = idem_key.as_ref().map(|client_key| {
                // A deployment's pick is in the payload; fingerprinting that would make a retry that
                // picks the other target a 409. The caller's own model string stands in for it.
                let payload = if req.model.starts_with("deployment/") {
                    let mut caller_view = gw.payload.clone();
                    caller_view["model"] = json!(req.model);
                    std::borrow::Cow::Owned(caller_view)
                } else {
                    std::borrow::Cow::Borrowed(&gw.payload)
                };
                crate::gateway::idempotency::IdempotencyContext::new(
                    &identity.tenant,
                    &identified_caller.credential_scope,
                    uri.path(),
                    client_key,
                    &payload,
                )
            });
        }
        tried.push(
            gw.payload
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        #[cfg_attr(not(feature = "serve"), allow(unused_mut))]
        let mut policy_scope = match state.trusted_policy_scope(identity).await {
            Ok(scope) => scope,
            Err(error) => {
                return Err(retry_or(
                    &state,
                    &mut previous,
                    gateway_err_to_api(&state, error),
                ))
            }
        };
        #[cfg(feature = "serve")]
        if state.attempt_coordinator.records_usage() {
            policy_scope.attribute(
                &identified_caller.credential_scope,
                request_id_of(&headers),
                req.model.strip_prefix("deployment/"),
            );
        }
        state.pin_router(&mut policy_scope, snapshot.clone());
        let prepared_submission = match state.prepare_submission(gw, policy_scope, false) {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(retry_or(
                    &state,
                    &mut previous,
                    gateway_err_to_api(&state, error),
                ))
            }
        };

        // Retry-safety: a repeated Idempotency-Key returns the first result.
        if first_attempt {
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
        }

        let timer = RequestTimer::start();
        let permit = prequeue_permit
            .take()
            .expect("a preparation permit is held for every attempt");
        match state
            .submit_prepared(prepared_submission, permit, logical_deadline)
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
                return Ok(
                    Json(value_to_response(&submitted.value, &provider_name, elapsed))
                        .into_response(),
                );
            }
            Err(err) => {
                let next = failover_target(&req.model, &snapshot, &tried, &err);
                let Some((name, target)) = next else {
                    return Err(gateway_err_to_api(&state, err));
                };
                previous = Some(err);
                // Counted when the retry has been prepared (below), not here: a retry the key may not
                // make, or that cannot get a permit, is not a failover.
                pending_failover = Some((
                    name.to_owned(),
                    tried.last().cloned().unwrap_or_default(),
                    target.clone(),
                ));
                forced = Some(target);
                match prequeue_permit_for_retry(&state).await {
                    Ok(permit) => prequeue_permit = Some(permit),
                    Err(_) => {
                        return Err(gateway_err_to_api(
                            &state,
                            previous.take().expect("the failure that led to this retry"),
                        ))
                    }
                }
            }
        }
    }
}

/// The deployment and the next untried target to send a request to after `error`, if it may be
/// retried at all (note 0031): the request addressed a deployment, the failure is the provider's, and
/// fewer than `min(targets, 3)` targets have been tried.
fn failover_target<'a>(
    model: &'a str,
    snapshot: &Router,
    tried: &[String],
    error: &GatewayError,
) -> Option<(&'a str, String)> {
    let name = model
        .strip_prefix("deployment/")
        .filter(|_| retryable_upstream_failure(error))
        .filter(|name| tried.len() < snapshot.deployment_target_count(name).min(3))?;
    snapshot
        .deployment_retry_target(name, tried)
        .map(|target| (name, target))
}

/// Whether a failed attempt says nothing about the request, so another target may be tried: a 5xx
/// from the provider or a transport failure (note 0031). A 429 and every local refusal (the
/// caller's own limits and budget, a policy refusal, an overload) are not: they are not the
/// target's fault, or they would only move the load. The strings are the `Display` of
/// `ShimError`, pinned by a test.
fn retryable_upstream_failure(error: &GatewayError) -> bool {
    let GatewayError::Upstream(message) = error else {
        return false;
    };
    if message.starts_with("HTTP error:") {
        return true;
    }
    let Some((status, body)) = message
        .strip_prefix("provider error (")
        .and_then(|rest| rest.split_once("): "))
    else {
        return false;
    };
    status
        .parse::<u16>()
        .is_ok_and(|status| matches!(status, 500 | 502 | 503 | 504 | 529))
        && !is_local_diagnostic(body)
}

/// llmshim's own 5xx diagnostics, raised **after** the provider answered with a success (an answer
/// that was billed but that cannot be used: an output contract it did not meet, a response shape
/// or terminal reason that is unsupported, a tool call that does not validate) or by the policy
/// layer. They say nothing about the target's health, and every target would give the same answer
/// again, so they are not retried. A new local diagnostic of this kind belongs here.
fn is_local_diagnostic(body: &str) -> bool {
    const LOCAL: &[&str] = &[
        "response did not satisfy the requested output contract",
        "upstream JSON exceeds complexity limit",
        "invalid upstream tool call",
        "invalid upstream response shape",
        "embeddings response has no data array",
        "no candidates in response",
        "no supported terminal",
        "no output in response",
        "returned an embeddings response but has no embeddings wire",
        "attempt policy observation failed",
        "attempt policy coordinator unavailable",
    ];
    LOCAL.iter().any(|known| body.contains(known))
        || body == crate::derived_response::DERIVED_RESPONSE_ERROR
}

/// The failure to report when a retry could not even be made: the one that led to the retry, when
/// there is one (note 0031), otherwise `error` itself.
fn retry_or(
    state: &GatewayState,
    previous: &mut Option<GatewayError>,
    error: ApiError,
) -> ApiError {
    match previous.take() {
        Some(failure) => gateway_err_to_api(state, failure),
        None => error,
    }
}

/// A fresh preparation permit for a retry (the first attempt's was spent queueing).
async fn prequeue_permit_for_retry(
    state: &GatewayState,
) -> Result<PrequeuePreparationPermit, ApiError> {
    prequeue_permit(state, None).await
}

/// A 400 in the OpenAI error shape, naming the field and never echoing a value.
fn invalid_request(message: &str, param: &str) -> ApiError {
    ApiError::from(ShimError::ProviderError {
        status: StatusCode::BAD_REQUEST.as_u16(),
        body: json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": param,
                "code": "invalid_request"
            }
        })
        .to_string(),
        retry_after: None,
    })
}

/// The inbound `POST /v1/embeddings` body, in OpenAI's shape.
#[derive(serde::Deserialize)]
struct EmbeddingsBody {
    model: String,
    input: Value,
    #[serde(default)]
    dimensions: Option<u32>,
    #[serde(default)]
    encoding_format: Option<String>,
}

/// The texts of an embeddings `input`: one string or an array of strings. Token arrays are not
/// supported: a vector from tokens of a tokenizer llmshim does not know is a wrong answer.
fn embedding_texts(input: &Value) -> Result<Vec<String>, ApiError> {
    match input {
        Value::String(text) => Ok(vec![text.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                invalid_request("input must be a string or an array of strings", "input")
            }),
        _ => Err(invalid_request(
            "input must be a string or an array of strings",
            "input",
        )),
    }
}

/// POST /v1/embeddings — OpenAI-shaped, admitted by the same rate, concurrency, spend and
/// allowlist rules as a completion (note 0035). Local scheduler only: a fleet answers 501.
async fn embeddings(
    State(state): State<Arc<GatewayState>>,
    installed_prequeue_permit: Option<axum::Extension<PrequeuePreparationPermit>>,
    logical_lifetime: Option<axum::Extension<crate::proxy::lifetime::LogicalRequestLifetime>>,
    headers: HeaderMap,
    Json(body): Json<EmbeddingsBody>,
) -> Result<Response, ApiError> {
    let prequeue_permit = prequeue_permit(&state, installed_prequeue_permit).await?;
    let identified_caller = state.keystore.identify_caller(&headers).map_err(|_| {
        crate::gateway::metrics::incr(
            crate::gateway::metrics::REJECTED,
            &[("provider", "unknown"), ("reason", "unauthorized")],
        );
        ApiError::Unauthorized
    })?;
    if !matches!(state.backend, Backend::Local(_)) {
        return Err(ApiError::from(ShimError::ProviderError {
            status: StatusCode::NOT_IMPLEMENTED.as_u16(),
            body: json!({
                "error": {
                    "message": "embeddings are not available on a fleet gateway",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "not_implemented"
                }
            })
            .to_string(),
            retry_after: None,
        }));
    }
    let base64 = match body.encoding_format.as_deref() {
        None | Some("float") => false,
        Some("base64") => true,
        Some(_) => {
            return Err(invalid_request(
                "encoding_format must be float or base64",
                "encoding_format",
            ))
        }
    };
    let texts = embedding_texts(&body.input)?;
    let request = crate::embeddings::EmbeddingRequest {
        model: body.model.clone(),
        input: texts,
        dimensions: body.dimensions,
    };
    let snapshot = state.router.current();
    // A route or deployment name is not an embeddings model: it resolves to a chat target.
    let (provider, model) = snapshot.resolve(&request.model).map_err(ApiError::from)?;
    let identity = &identified_caller.identity;
    if identity.allowed_models.is_some() {
        let (provider_key, routed) = snapshot
            .routing_id(&request.model)
            .map_err(ApiError::from)?;
        if !identity.permits_model(&provider_key, &routed, None) {
            crate::gateway::metrics::incr(
                crate::gateway::metrics::REJECTED,
                &[("provider", &provider_key), ("reason", "model_not_allowed")],
            );
            return Err(model_not_allowed());
        }
    }
    let mut payload = crate::embeddings::payload(&request);
    payload["model"] = json!(request.model);
    // The dispatch's own three refusals, asked here too (cheap, pure, same order) so a request no
    // provider could accept is a 400 before it queues, not a 502 out of the dispatcher.
    crate::embeddings::admit_batch(provider, &payload).map_err(ApiError::from)?;
    provider
        .transform_embedding_request(&model, &payload)
        .map_err(ApiError::from)?;
    crate::embeddings::admit_model(provider, &model, &payload).map_err(ApiError::from)?;
    let permits = request
        .input
        .iter()
        .map(|text| text.len() as u64 / 4 + 1)
        .sum::<u64>()
        .min(u64::from(u32::MAX)) as u32;
    let gw = GatewayRequest {
        provider: provider.instance().to_owned(),
        tier: identity.tier,
        permits,
        payload,
    };
    #[cfg_attr(not(feature = "serve"), allow(unused_mut))]
    let mut policy_scope = state
        .trusted_policy_scope(identity)
        .await
        .map_err(|error| gateway_err_to_api(&state, error))?;
    #[cfg(feature = "serve")]
    if state.attempt_coordinator.records_usage() {
        policy_scope.attribute(
            &identified_caller.credential_scope,
            request_id_of(&headers),
            None,
        );
    }
    state.pin_router(&mut policy_scope, snapshot.clone());
    policy_scope.set_operation(crate::policy::DispatchOperation::Embeddings);
    let prepared = state
        .prepare_submission(gw, policy_scope, false)
        .map_err(|error| gateway_err_to_api(&state, error))?;
    let submitted = state
        .submit_prepared(
            prepared,
            prequeue_permit,
            logical_lifetime
                .as_ref()
                .map(|axum::Extension(lifetime)| lifetime.deadline()),
        )
        .await
        .map_err(|error| gateway_err_to_api(&state, error))?;
    let parsed = crate::embeddings::parse(
        provider.instance(),
        &model,
        request.input.len(),
        submitted.value,
    )
    .map_err(ApiError::from)?;
    Ok(Json(embeddings_response(&body.model, &parsed, base64)).into_response())
}

/// OpenAI's list-of-embeddings answer. `base64` packs each vector as little-endian f32, which is
/// what the OpenAI SDKs ask for by default and decode. Cost rides on `usage`; `null` is unknown.
fn embeddings_response(
    requested_model: &str,
    embeddings: &crate::embeddings::Embeddings,
    base64: bool,
) -> Value {
    use base64::Engine as _;
    let data: Vec<Value> = embeddings
        .vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| {
            let embedding = if base64 {
                let bytes: Vec<u8> = vector
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect();
                json!(base64::engine::general_purpose::STANDARD.encode(bytes))
            } else {
                json!(vector)
            };
            json!({"object": "embedding", "index": index, "embedding": embedding})
        })
        .collect();
    let tokens = embeddings.usage.input_tokens;
    json!({
        "object": "list",
        "data": data,
        "model": embeddings.revision.as_deref().unwrap_or(requested_model),
        "usage": {
            "prompt_tokens": tokens,
            "total_tokens": tokens,
            "cost_usd": embeddings.usage.cost_usd,
            "cost_source": embeddings.usage.cost_source,
        }
    })
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
    chat_stream_inner(
        state,
        headers,
        req,
        prequeue_permit,
        logical_lifetime,
        false,
        None,
    )
    .await
}

async fn chat_stream_inner(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    req: ChatRequest,
    prequeue_permit: PrequeuePreparationPermit,
    logical_lifetime: Option<axum::Extension<crate::proxy::lifetime::LogicalRequestLifetime>>,
    responses_identity: bool,
    preselected: Option<Arc<Router>>,
) -> Response {
    // A stream that fails to open (a 5xx or a transport failure before the first byte) is retried on
    // another target of its deployment exactly as a unary request is (note 0031): nothing has been
    // sent to the caller yet, and each retry is a new submission with its own admission and record.
    let logical_deadline = logical_lifetime
        .as_ref()
        .map(|axum::Extension(lifetime)| lifetime.deadline());
    let mut tried: Vec<String> = Vec::new();
    let mut forced: Option<String> = None;
    let mut preselected = preselected;
    let mut prequeue_permit = Some(prequeue_permit);
    let mut previous: Option<GatewayError> = None;
    let mut pending_failover: Option<(String, String, String)> = None;
    let mut rx = loop {
        let (_provider_name, _budget_model, gw, identified_caller, snapshot) =
            match build_request_for(
                &state,
                &headers,
                &req,
                preselected.clone(),
                forced.as_deref(),
            ) {
                Ok(built) => built,
                Err(error) => return retry_or(&state, &mut previous, error).into_response(),
            };
        preselected = Some(snapshot.clone());
        if let Some((name, from, to)) = pending_failover.take() {
            crate::gateway::metrics::incr(
                crate::gateway::metrics::DEPLOYMENT_FAILOVER,
                &[("deployment", &name), ("from", &from), ("to", &to)],
            );
        }
        let identity = identified_caller.identity;
        tried.push(
            gw.payload
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        #[cfg_attr(not(feature = "serve"), allow(unused_mut))]
        let mut policy_scope = match state.trusted_policy_scope(&identity).await {
            Ok(scope) => scope,
            Err(error) => {
                return retry_or(&state, &mut previous, gateway_err_to_api(&state, error))
                    .into_response()
            }
        };
        #[cfg(feature = "serve")]
        if state.attempt_coordinator.records_usage() {
            policy_scope.attribute(
                &identified_caller.credential_scope,
                request_id_of(&headers),
                req.model.strip_prefix("deployment/"),
            );
        }
        state.pin_router(&mut policy_scope, snapshot.clone());
        let prepared_submission = match state.prepare_submission(gw, policy_scope, true) {
            Ok(prepared) => prepared,
            Err(error) => {
                return retry_or(&state, &mut previous, gateway_err_to_api(&state, error))
                    .into_response()
            }
        };
        // Admission (queue + rate) happens up front so a rejection is a proper
        // 429/503 before the SSE response begins, not an SSE error event.
        let permit = prequeue_permit
            .take()
            .expect("a preparation permit is held for every attempt");
        match state
            .submit_stream_prepared(prepared_submission, permit, logical_deadline)
            .await
        {
            Ok(rx) => break rx,
            Err(err) => {
                let Some((name, target)) = failover_target(&req.model, &snapshot, &tried, &err)
                else {
                    return gateway_err_to_api(&state, err).into_response();
                };
                pending_failover = Some((
                    name.to_owned(),
                    tried.last().cloned().unwrap_or_default(),
                    target.clone(),
                ));
                previous = Some(err);
                forced = Some(target);
                match prequeue_permit_for_retry(&state).await {
                    Ok(permit) => prequeue_permit = Some(permit),
                    Err(_) => {
                        return gateway_err_to_api(
                            &state,
                            previous.take().expect("the failure that led to this retry"),
                        )
                        .into_response()
                    }
                }
            }
        }
    };

    let event_stream = async_stream::stream! {
        let mut identity_sent = false;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(chunk) => {
                    if responses_identity && !identity_sent {
                        identity_sent = true;
                        if let Some(event) = crate::proxy::wire::stream_identity(&chunk) {
                            yield Ok(event);
                        }
                    }
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
        &state.router.current().provider_keys(),
        &state.router.current().served_models(),
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

/// The id a request that sent none gets. Under `serve` it is
/// `gw-<instance>-<n>`: `<instance>` is 12 random hex digits drawn once per
/// process and `<n>` a per-process counter, so ids do not repeat across replicas
/// or restarts. (A build without `serve` keeps `gw-<pid>-<n>`, which is unique within
/// one process only: in a container the pid is often 1.)
fn minted_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "serve")]
    {
        static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let instance = INSTANCE.get_or_init(new_instance_id);
        format!("gw-{instance}-{n}")
    }
    #[cfg(not(feature = "serve"))]
    {
        format!("gw-{}-{n}", std::process::id())
    }
}

/// 12 random hex digits, drawn once per process.
#[cfg(feature = "serve")]
fn new_instance_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].into()
}

/// The request's id as the middleware settled it, for handlers.
#[cfg(feature = "serve")]
fn request_id_of(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-request-id").and_then(|v| v.to_str().ok())
}

/// Whether a caller's own `x-request-id` is kept: 1 to 128 visible ASCII
/// characters, and not in the gateway's own `gw-` form (a caller cannot take the
/// id of another request, minted or future). Anything else is replaced by a
/// minted id, never echoed. (Only under `serve`; a build without it keeps whatever
/// header it was sent.)
#[cfg(feature = "serve")]
fn well_formed_request_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_graphic())
        && !id.starts_with("gw-")
}

/// The id a request has already settled on, put in its extensions by the
/// outermost layer (the access log) so the gateway's own layer keeps it.
#[cfg(feature = "serve")]
#[derive(Clone)]
pub(crate) struct SettledRequestId(pub(crate) String);

/// Settle a request's id: the caller's own if it is well formed, else a minted one.
#[cfg(feature = "serve")]
pub(crate) fn settle_request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|id| well_formed_request_id(id))
        .map(str::to_string)
        .unwrap_or_else(minted_request_id)
}

/// Ensure every request/response carries an `x-request-id` for correlation
/// (honoring an incoming one, else minting a unique id).
async fn request_id(mut req: Request, next: Next) -> Response {
    #[cfg(feature = "serve")]
    let id = req
        .extensions()
        .get::<SettledRequestId>()
        .map(|settled| settled.0.clone())
        .unwrap_or_else(|| settle_request_id(req.headers()));
    #[cfg(not(feature = "serve"))]
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(minted_request_id);
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
            .current()
            .provider_keys()
            .into_iter()
            .map(String::from)
            .collect(),
    })
}

/// The operational endpoints alone, for a listener of their own. These are the
/// same handlers the main application serves at the same paths.
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
pub(crate) fn admin_routes(state: Arc<GatewayState>) -> axum::Router {
    axum::Router::new()
        .route("/v1/gateway/stats", get(stats))
        .route("/metrics", get(metrics))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .layer(axum::middleware::from_fn(request_id))
        .with_state(state)
}

/// Paths that [`admin_routes`] serves.
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
pub(crate) const ADMIN_PATHS: [&str; 4] = ["/v1/gateway/stats", "/metrics", "/health", "/ready"];

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
    app_with_parts(
        state,
        origin_policy,
        logical_deadlines,
        crate::proxy::wire::DefaultReceiptStore::from_env(),
    )
}

/// Build the gateway application from every input it needs, reading nothing
/// from the environment. The `app*` functions above are this with
/// environment-derived values.
pub(crate) fn app_with_parts(
    state: Arc<GatewayState>,
    origin_policy: OriginPolicy,
    logical_deadlines: crate::proxy::lifetime::LogicalRequestDeadlines,
    default_receipt_store: crate::proxy::wire::DefaultReceiptStore,
) -> axum::Router {
    let ingress_state = IngressAdmissionState {
        gateway: state.clone(),
        deadlines: logical_deadlines,
    };
    axum::Router::new()
        .route("/v1/chat", post(chat))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(chat))
        .route("/v1/responses", post(chat))
        .route("/v1beta/models/{*rest}", post(chat))
        .route("/v1/chat/stream", post(chat_stream))
        .route("/v1/embeddings", post(embeddings))
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
        && (matches!(
            inference_path.as_str(),
            "/v1/chat" | "/v1/chat/stream" | "/v1/embeddings"
        ) || crate::proxy::wire::native_route(&inference_path).is_some());
    if !requires_preparation {
        return next.run(request).await;
    }

    let native_wire = crate::proxy::wire::native_route(&inference_path).map(|route| route.wire);
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

/// The router the native-route translation layer resolved against, handed on to
/// admission so both use one snapshot (0022).
#[derive(Clone)]
struct AdmittedRouter(Arc<Router>);

async fn native_translate(
    State(state): State<Arc<GatewayState>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let wire = crate::proxy::wire::native_route(request.uri().path()).map(|route| route.wire);
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
    // One snapshot for translation and admission: the replay target translation
    // builds and the router the request is admitted on must be the same one.
    let snapshot = state.router.current();
    request
        .extensions_mut()
        .insert(AdmittedRouter(snapshot.clone()));
    crate::proxy::wire::translate_with_router(request, next, Some(&snapshot)).await
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
                allowed_models: None,
                key_name: None,
                tenant_budget: None,
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
                router: RouterHandle::from_arc(router.clone()),
                logger: None,
                penalty: None,
            }),
        );
        let keys = std::collections::HashMap::from([
            ("test-key".into(), identity.clone()),
            ("second-key".into(), identity),
        ]);
        Arc::new(GatewayState {
            router: RouterHandle::from_arc(router),
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
            #[cfg(feature = "redis-coordination")]
            outage: None,
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
                router: RouterHandle::from_arc(router.clone()),
                logger: None,
                penalty: None,
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
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
        };
        Arc::new(GatewayState {
            router: RouterHandle::from_arc(router),
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
            #[cfg(feature = "redis-coordination")]
            outage: None,
        })
    }

    fn refusal_request(path: &str, stream: bool) -> Request<Body> {
        // Gemini carries its turns in `contents` and its action in the path, so
        // the refusal has to reach the wire's own body check to be its refusal.
        let body = if path.starts_with("/v1beta/") {
            json!({"contents": [{"role": "user","parts": [{"text": "hi"}]}]})
        } else {
            json!({
                "model": "local/test",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": stream
            })
        };
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-key")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn assert_gateway_refusal_surfaces(state: Arc<GatewayState>, expects_retry_after: bool) {
        for (path, stream, expected_native) in [
            ("/v1/chat", false, None),
            ("/v1/chat/stream", true, None),
            (
                "/v1/chat/completions",
                false,
                Some(("/error/type", "api_error")),
            ),
            (
                "/v1/chat/completions",
                true,
                Some(("/error/type", "api_error")),
            ),
            (
                "/v1/messages",
                false,
                Some(("/error/type", "overloaded_error")),
            ),
            (
                "/v1/messages",
                true,
                Some(("/error/type", "overloaded_error")),
            ),
            (
                "/v1beta/models/local/test:generateContent",
                false,
                Some(("/error/status", "UNAVAILABLE")),
            ),
            (
                "/v1beta/models/local/test:streamGenerateContent",
                true,
                Some(("/error/status", "UNAVAILABLE")),
            ),
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
            if let Some((pointer, expected_native)) = expected_native {
                assert_eq!(
                    body.pointer(pointer).and_then(Value::as_str),
                    Some(expected_native),
                    "{path}, stream={stream}"
                );
            }
        }
    }

    #[tokio::test]
    async fn excessive_typed_reasoning_is_rejected_before_gateway_queue_or_dispatch() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let details: Vec<Value> = (0..64)
            .map(|index| json!({"type": "reasoning.text","text": "x","index": index}))
            .collect();
        let body = json!({
            "model": "local/gpt-5.6-luna",
            "messages": [{
                "role": "assistant",
                "content": "answer",
                "reasoning_origin": {
                    "provider": "source",
                    "model": "m".repeat(64 * 1024),
                    "family": "claude",
                    "wire": "openai-chat",
                    "received_at": "2026-09-22T00:00:00Z"
                },
                "reasoning_details": details
            }]
        });
        assert!(body.to_string().len() < 80 * 1024);
        for stream in [false, true] {
            let mut request_body = body.clone();
            request_body["stream"] = json!(stream);
            let response = app(configured_state(&server.url()))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/chat")
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(request_body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert!(String::from_utf8(response_body.to_vec())
                .unwrap()
                .contains("request reasoning metadata exceeds derived size limit"));
        }
        upstream.assert_async().await;
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
            Duration::from_secs(15),
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
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
        };
        Arc::new(GatewayState {
            router: RouterHandle::from_arc(router),
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
            #[cfg(feature = "redis-coordination")]
            outage: None,
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
                            "messages": [{"role": "user","content": "quiet"}],
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
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
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
            router: RouterHandle::new(Router::new()),
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
            #[cfg(feature = "redis-coordination")]
            outage: None,
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
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
        };
        let state = Arc::new(GatewayState {
            router: RouterHandle::from_arc(router),
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
            #[cfg(feature = "redis-coordination")]
            outage: None,
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
                json!({
                    "id": "a",
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "private-a"
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                    }
                })
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
                json!({
                    "id": "b",
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "private-b"
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                    }
                })
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
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "unexpected"
                            },
                            "finish_reason": "stop"
                        }
                    ],
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
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
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
            json!({
                "id": "r",
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "content": "ok"
                        },
                        "finish_reason": null
                    }
                ]
            }),
            json!({
                "id": "r",
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                        },
                        "finish_reason": null
                    }
                ],
                "usage": {
                    "prompt_tokens": 7
                }
            }),
            json!({"id": "r","choices": [{"index": 0,"delta": {},"finish_reason": "stop"}]})
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
                json!({"choices": [],"usage": {"prompt_tokens": 7,"completion_tokens": 3}}),
                json!({
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "content": "later"
                            },
                            "finish_reason": null
                        }
                    ]
                }),
                json!({"choices": [{"index": 0,"delta": {},"finish_reason": "stop"}]}),
            ],
            vec![
                json!({"choices": [
                    {"index": 0,"delta": {},"finish_reason": "stop"},
                    {"index": 1,"delta": {},"finish_reason": null}
                ],"usage": {"prompt_tokens": 7,"completion_tokens": 3}}),
                json!({
                    "choices": [
                        {
                            "index": 1,
                            "delta": {
                                "content": "later"
                            },
                            "finish_reason": null
                        }
                    ]
                }),
                json!({"choices": [{"index": 1,"delta": {},"finish_reason": "stop"}]}),
            ],
            vec![
                json!({
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 7,
                        "completion_tokens": 3
                    }
                }),
                json!({
                    "choices": [
                        {
                            "index": 1,
                            "delta": {
                                "content": "new"
                            },
                            "finish_reason": null
                        }
                    ]
                }),
                json!({"choices": [{"index": 1,"delta": {},"finish_reason": "stop"}]}),
            ],
        ];
        for sequence in chat_unsafe {
            assert_chat_stream_budget_finality(sequence, StatusCode::TOO_MANY_REQUESTS).await;
        }
        assert_chat_stream_budget_finality(
            vec![
                json!({"choices": [{"index": 0,"delta": {"content": "ok"},"finish_reason": null}]}),
                json!({"choices": [{"index": 0,"delta": {},"finish_reason": "stop"}]}),
                json!({"choices": [],"usage": {"prompt_tokens": 7,"completion_tokens": 3}}),
            ],
            StatusCode::OK,
        )
        .await;
        assert_chat_stream_budget_finality(
            vec![
                json!({
                    "choices": [{"index": 0,"delta": {"content": "ok"},"finish_reason": null}],
                    "usage": {"cost": 1.0}
                }),
                json!({"choices": [{"index": 0,"delta": {},"finish_reason": "stop"}]}),
                json!({"choices": [],"usage": {"prompt_tokens": 7,"completion_tokens": 3}}),
            ],
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
        assert_chat_stream_budget_finality(
            vec![
                json!({
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                        "cost": 0.25
                    }
                }),
                json!({"choices": [],"usage": {"prompt_tokens": 1_000_000,"completion_tokens": 0}}),
            ],
            StatusCode::OK,
        )
        .await;
        assert_chat_stream_budget_finality(
            vec![
                json!({
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                        "cost": 0.25
                    }
                }),
                json!({
                    "choices": [
                        {
                            "index": 1,
                            "delta": {
                                "content": "later"
                            },
                            "finish_reason": null
                        }
                    ]
                }),
                json!({"choices": [{"index": 1,"delta": {},"finish_reason": "stop"}]}),
                json!({"choices": [],"usage": {"prompt_tokens": 1_000_000,"completion_tokens": 0}}),
            ],
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;

        let gemini_unsafe = vec![
            vec![
                json!({
                    "candidates": [
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 7,
                        "candidatesTokenCount": 3,
                        "cost": 1.0
                    }
                }),
                json!({"candidates": [{"index": 0,"content": {"parts": [{"text": "later"}]}}]}),
                json!({
                    "candidates": [
                        {
                            "index": 0,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ]
                }),
            ],
            vec![
                json!({"candidates": [
                    {"index": 0,"finishReason": "STOP","content": {"parts": []}},
                    {"index": 1,"content": {"parts": []}}
                ],"usageMetadata": {"promptTokenCount": 7,"candidatesTokenCount": 3,"cost": 1.0}}),
                json!({"candidates": [{"index": 1,"content": {"parts": [{"text": "later"}]}}]}),
                json!({
                    "candidates": [
                        {
                            "index": 1,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ]
                }),
            ],
            vec![
                json!({
                    "candidates": [
                        {
                            "index": 0,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 7,
                        "candidatesTokenCount": 3,
                        "cost": 1.0
                    }
                }),
                json!({"candidates": [{"index": 1,"content": {"parts": [{"text": "new"}]}}]}),
                json!({
                    "candidates": [
                        {
                            "index": 1,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ]
                }),
            ],
        ];
        for sequence in gemini_unsafe {
            assert_gemini_stream_budget_finality(sequence, StatusCode::TOO_MANY_REQUESTS).await;
        }
        assert_gemini_stream_budget_finality(
            vec![
                json!({"candidates": [{"index": 0,"content": {"parts": [{"text": "ok"}]}}]}),
                json!({
                    "candidates": [
                        {
                            "index": 0,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ]
                }),
                json!({
                    "candidates": [
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 7,
                        "candidatesTokenCount": 3,
                        "cost": 1.0
                    }
                }),
            ],
            StatusCode::OK,
        )
        .await;
        assert_gemini_stream_budget_finality(
            vec![
                json!({
                    "candidates": [
                        {
                            "index": 0,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ],
                    "usageMetadata": {
                        "cost": 0.25
                    }
                }),
                json!({
                    "candidates": [
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 7,
                        "candidatesTokenCount": 3
                    }
                }),
            ],
            StatusCode::OK,
        )
        .await;
        assert_gemini_stream_budget_finality(
            vec![
                json!({
                    "candidates": [
                        {
                            "index": 0,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ],
                    "usageMetadata": {
                        "cost": 0.25
                    }
                }),
                json!({"candidates": [{"index": 1,"content": {"parts": [{"text": "later"}]}}]}),
                json!({
                    "candidates": [
                        {
                            "index": 1,
                            "finishReason": "STOP",
                            "content": {
                                "parts": [
                                ]
                            }
                        }
                    ]
                }),
                json!({
                    "candidates": [
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 7,
                        "candidatesTokenCount": 3
                    }
                }),
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
                    router: RouterHandle::from_arc(router.clone()),
                    logger: None,
                    penalty: None,
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
                allowed_models: None,
                key_name: None,
                tenant_budget: None,
            };
            Arc::new(GatewayState {
                router: RouterHandle::from_arc(router),
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
                #[cfg(feature = "redis-coordination")]
                outage: None,
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
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "r",
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "hello"
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                    }
                })
                .to_string(),
            )
            .expect(2)
            .create_async()
            .await;
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
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("x-api-key", "test-key")
                        .header("x-llmshim-priority", "255")
                        .header("idempotency-key", "same-client-key")
                        .body(Body::from(
                            json!({
                                "model": "local/test",
                                "messages": [
                                    {
                                        "role": "user",
                                        "content": "hi"
                                    }
                                ]
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().contains_key("x-request-id"));
            let body = to_bytes(response.into_body(), 10000).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("hello"));
        }
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn gemini_native_route_accepts_googles_key_header() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "r",
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "hello"
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                    }
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let application = app(configured_state(&server.url())).layer(Extension(Arc::new(
            crate::proxy::wire::Receipts::new(dir.path().to_owned()),
        )));
        // Gemini SDKs send the key in `x-goog-api-key`, not `Authorization`.
        let request = |key: Option<&str>| {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/v1beta/models/local/test:generateContent")
                .header("content-type", "application/json");
            if let Some(key) = key {
                builder = builder.header("x-goog-api-key", key);
            }
            builder
                .body(Body::from(
                    json!({"contents": [{"role": "user","parts": [{"text": "hi"}]}]}).to_string(),
                ))
                .unwrap()
        };

        let rejected = application.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        let error: Value =
            serde_json::from_slice(&to_bytes(rejected.into_body(), 10_000).await.unwrap()).unwrap();
        assert_eq!(error["error"]["code"], 401);
        assert_eq!(error["error"]["status"], "UNAUTHENTICATED");

        let accepted = application
            .oneshot(request(Some("test-key")))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(accepted.into_body(), 100_000).await.unwrap())
                .unwrap();
        assert_eq!(
            body["candidates"][0]["content"]["parts"][0]["text"],
            "hello"
        );
        assert_eq!(body["candidates"][0]["finishReason"], "STOP");
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn native_idempotency_request_mismatch_is_a_native_shaped_conflict() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "r",
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "hello"
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                    }
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
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
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "r",
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "private-cached-content"
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                    }
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
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
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(401)
            .with_body(
                json!({
                    "error": {
                        "type": "invalid_request_error",
                        "code": "invalid_api_key",
                        "message": "API key is invalid.",
                        "param": null
                    }
                })
                .to_string(),
            )
            .expect(5)
            .create_async()
            .await;
        let state = configured_state(&server.url());
        let application = app(state);
        for (path, stream) in [
            ("/v1/chat", false),
            ("/v1/chat", true),
            ("/v1/chat/stream", true),
            ("/v1/messages", false),
            ("/v1/chat/completions", false),
        ] {
            let response = application
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(
                            json!({
                                "model": "local/test",
                                "messages": [
                                    {
                                        "role": "user",
                                        "content": "hi"
                                    }
                                ],
                                "stream": stream
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
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
            let response = application
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-key")
                        .body(Body::from(
                            json!({
                                "model": "local/test",
                                "messages": [
                                    {
                                        "role": "assistant",
                                        "content": "answer",
                                        "tool_calls": {
                                        }
                                    }
                                ]
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
                            json!([
                                {
                                    "type": "function",
                                    "function": {
                                        "name": "lookup",
                                        "parameters": {
                                            "type": "object"
                                        }
                                    }
                                }
                            ]),
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
            "model": "route/large",
            "messages": [{"role": "user","content": "hello"}]
        }))
        .unwrap();
        let (provider_name, budget_model, gateway_request, _, _) =
            match build_request(&state, &headers, &request, None) {
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
                    "model": "local/declared",
                    "messages": [{"role": "user","content": "canonical"}],
                    "provider_config": {"x-local": {(protected_field):"replacement"}}
                }))
                .unwrap();
                assert!(build_request(&state, &headers, &request, None).is_err());
            }

            let request: ChatRequest = serde_json::from_value(json!({
                "model": "local/declared",
                "messages": [{"role": "user","content": "canonical"}],
                "provider_config": {"x-local": {(output_field):7_000}}
            }))
            .unwrap();
            let (_, _, gateway_request, _, _) =
                match build_request(&state, &headers, &request, None) {
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

    /// A mock upstream that answers one chat completion saying who it is.
    async fn labelled_upstream(label: &str, calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_body(
                json!({"id": "c", "created": 1, "model": "m",
                    "choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": label}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
                .to_string(),
            )
            .expect(calls)
            .create_async()
            .await;
        (server, mock)
    }

    fn router_to(url: String) -> Router {
        Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local", url, None,
            )),
        )
    }

    fn chat_payload() -> Value {
        json!({"model": "local/m", "messages": [{"role": "user", "content": "hi"}]})
    }

    #[tokio::test]
    async fn a_request_admitted_on_one_router_runs_on_it_after_the_router_is_replaced() {
        // 0022: admission pins the router in force; a swap afterwards (a reload)
        // changes what the next request uses and nothing already admitted.
        let (a, on_a) = labelled_upstream("from A", 1).await;
        let (b, on_b) = labelled_upstream("from B", 1).await;
        let handle = RouterHandle::new(router_to(a.url()));
        let dispatch = RealDispatch {
            router: handle.clone(),
            logger: None,
            penalty: None,
        };
        let state = configured_state_with_router(Router::new());
        let identity = crate::gateway::auth::Identity {
            tenant: "t".into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
        };
        // Admitted against A...
        let mut scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(&identity);
        scope.set_router(handle.current());
        let admitted = state.attempt_coordinator.context(scope);
        // ...then a reload puts B in force.
        handle.replace(router_to(b.url()));
        let answered = dispatch
            .dispatch_with_policy("local", chat_payload(), admitted)
            .await
            .unwrap_or_else(|error| panic!("the admitted request still runs: {}", error.message));
        assert_eq!(answered["choices"][0]["message"]["content"], "from A");
        on_a.assert_async().await;
        // A request that carries no snapshot (or is admitted after the swap) uses B.
        let fresh = dispatch
            .dispatch("local", chat_payload())
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(fresh["choices"][0]["message"]["content"], "from B");
        on_b.assert_async().await;
    }

    /// POST /v1/chat through the real handler and the scheduler.
    async fn post_chat(app: axum::Router) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(chat_payload().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_queued_behind_another_runs_on_the_router_it_was_admitted_on() {
        // The handler pins the router at admission (0022): with one request in
        // flight and a second waiting its turn, a reload that replaces the router
        // in between changes neither. Delete the pin in the handler and the
        // second request is dispatched on B.
        let body = |label: &str| {
            json!({"id": "c", "created": 1, "model": "m",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": label}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
            .to_string()
        };
        let mut a = mockito::Server::new_async().await;
        let first_arrived = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slow_a = {
            let body = body("from A");
            let arrived = first_arrived.clone();
            a.mock("POST", "/chat/completions")
                .with_header("content-type", "application/json")
                .with_chunked_body(move |writer| {
                    arrived.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2000));
                    writer.write_all(body.as_bytes())
                })
                .expect(2)
                .create_async()
                .await
        };
        let (b, on_b) = labelled_upstream("from B", 0).await;
        let mut parts = LocalParts::from_env();
        parts.config.max_concurrency_per_provider = 1;
        let state = GatewayState::from_parts(router_to(a.url()), None, parts);
        let application = app(state.clone());
        // Wait on events, not sleeps: admission can be slow on a cold process.
        let first = tokio::spawn(post_chat(application.clone()));
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while !first_arrived.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < until,
                "the first request never reached A"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second = tokio::spawn(post_chat(application.clone()));
        // The second is admitted (cheap: the process is warm) and waits behind the
        // first, which holds A for two seconds; the margin is the wait, not a race.
        tokio::time::sleep(Duration::from_millis(800)).await;
        state.router.replace(router_to(b.url()));
        for (name, request) in [("first", first), ("second", second)] {
            let (status, answer) = request.await.unwrap();
            assert_eq!(status, StatusCode::OK, "{name}: {answer}");
            assert_eq!(
                answer["message"]["content"], "from A",
                "{name} ran on the router it was admitted on"
            );
        }
        slow_a.assert_async().await;
        on_b.assert_async().await;
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn a_replica_without_the_provider_answers_503_provider_unavailable() {
        // 0022: a fleet job reaches a replica whose router does not know the
        // provider (a roll-out, or a removal). That is local, not an upstream
        // failure: 503 with Retry-After, never 502, and it fails before any
        // attempt exists, so nothing was sent and no spend was reserved.
        let dispatch = RealDispatch {
            router: RouterHandle::new(Router::new()),
            logger: None,
            penalty: Some(Duration::from_secs(7)),
        };
        let error = dispatch
            .dispatch("gone", chat_payload_for("gone/m"))
            .await
            .unwrap_err();
        assert_eq!(error.message, "llmshim-provider-unavailable:7000");
        let state = configured_state_with_router(Router::new());
        let response =
            gateway_err_to_api(&state, GatewayError::Upstream(error.message)).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "7");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("provider_unavailable"));
    }

    #[cfg(feature = "serve")]
    fn chat_payload_for(model: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
    }

    #[test]
    fn the_admitted_router_rides_the_scope_into_the_context_and_never_through_redis() {
        let state = configured_state_with_router(Router::new());
        let identity = crate::gateway::auth::Identity {
            tenant: "t".into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
            allowed_models: None,
            key_name: None,
            tenant_budget: None,
        };
        let admitted = Arc::new(Router::new());
        let mut scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(&identity);
        scope.set_router(admitted.clone());
        // A job that crosses Redis carries the scope serialized: the snapshot is not
        // part of it, so the replica that runs the job uses its own router.
        let wire = serde_json::to_string(&scope).unwrap();
        let received: crate::gateway::attempt::TrustedPolicyScope =
            serde_json::from_str(&wire).unwrap();
        let context = state.attempt_coordinator.context(received);
        assert!(context.router_snapshot().is_none());
        let context = state.attempt_coordinator.context(scope);
        assert!(Arc::ptr_eq(context.router_snapshot().unwrap(), &admitted));
    }

    #[test]
    fn a_snapshot_survives_a_swap_and_the_swap_takes_effect_for_new_readers() {
        let handle = RouterHandle::new(Router::new());
        let before = handle.current();
        handle.replace(router_to("http://127.0.0.1:1".into()));
        assert!(
            before.provider_keys().is_empty(),
            "a snapshot is not edited"
        );
        assert_eq!(handle.current().provider_keys(), vec!["local"]);
    }

    #[tokio::test]
    async fn real_dispatch_assigns_identity_before_gateway_storage() {
        for supplied_identity in [false, true] {
            let mut server = mockito::Server::new_async().await;
            let mut body = json!({
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "Hello"
                        },
                        "finish_reason": "stop"
                    }
                ],
                "usage": {
                }
            });
            if supplied_identity {
                body["id"] = json!("chat_original");
                body["created"] = json!(123);
            }
            let upstream = server
                .mock("POST", "/chat/completions")
                .with_body(body.to_string())
                .expect(1)
                .create_async()
                .await;
            let dispatch = RealDispatch {
                router: RouterHandle::new(Router::new().register(
                    "local",
                    Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                        "local",
                        server.url(),
                        None,
                    )),
                )),
                logger: None,
                penalty: None,
            };
            let response = dispatch
                .dispatch(
                    "local",
                    json!({"model": "local/test","messages": [{"role": "user","content": "hi"}]}),
                )
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(response["choices"][0]["message"]["content"], "Hello");
            if supplied_identity {
                assert_eq!(response["id"], "chat_original");
                assert_eq!(response["created"], 123);
            } else {
                assert!(response["id"].as_str().unwrap().starts_with("chat_"));
                assert!(response["created_at"].as_i64().unwrap() > 0);
            }
            upstream.assert_async().await;
        }
    }
}

#[cfg(test)]
mod request_id_tests {
    use super::*;
    use tower::ServiceExt;

    /// Send one request through the request-id middleware and return the id on
    /// the response and the id the handler saw.
    async fn through(sent: Option<&[u8]>) -> (String, String) {
        let app = axum::Router::new()
            .route(
                "/",
                get(|headers: HeaderMap| async move {
                    headers
                        .get("x-request-id")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_owned()
                }),
            )
            .layer(axum::middleware::from_fn(request_id));
        let mut request = axum::http::Request::builder().uri("/");
        if let Some(bytes) = sent {
            request = request.header("x-request-id", HeaderValue::from_bytes(bytes).unwrap());
        }
        let response = app
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let on_response = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        (on_response, String::from_utf8(body.to_vec()).unwrap())
    }

    /// `(instance, counter)` of `gw-<instance>-<counter>`.
    #[cfg(feature = "serve")]
    fn parts(id: &str) -> (String, u64) {
        let (instance, counter) = id.strip_prefix("gw-").unwrap().split_once('-').unwrap();
        (instance.to_owned(), counter.parse().unwrap())
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn minted_ids_carry_an_instance_so_replicas_and_restarts_do_not_repeat_them() {
        let (first, seen) = through(None).await;
        assert_eq!(first, seen, "the handler sees the id the response carries");
        let (second, _) = through(None).await;
        let ((instance_a, a), (instance_b, b)) = (parts(&first), parts(&second));
        assert_eq!(instance_a.len(), 12, "{first}");
        assert!(
            instance_a.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{first}"
        );
        assert_eq!(instance_a, instance_b, "one process, one instance part");
        assert!(b > a, "the counter advances: {first} then {second}");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn each_process_draws_its_own_instance_part() {
        // The part that makes ids unique across replicas and restarts is drawn at
        // random, not derived from the pid (which is often 1 in a container).
        let drawn: std::collections::BTreeSet<String> =
            (0..64).map(|_| new_instance_id()).collect();
        assert_eq!(drawn.len(), 64, "64 draws, 64 different instance parts");
        assert!(drawn
            .iter()
            .all(|id| id.len() == 12 && id.bytes().all(|byte| byte.is_ascii_hexdigit())));
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn a_caller_cannot_take_an_id_in_the_gateways_own_form() {
        // The instance part is on every response and the counter is sequential,
        // so a caller could otherwise send the id of another request, or of one
        // not yet made, and pollute that request's totals.
        for taken in ["gw-3f9a1c0b7e42-12", "gw-anything", "gw-"] {
            let (on_response, seen) = through(Some(taken.as_bytes())).await;
            assert_ne!(on_response, taken, "{taken} is the gateway's to mint");
            assert!(on_response.starts_with("gw-"));
            assert_eq!(on_response, seen);
        }
        // Only the prefix is reserved.
        let (kept, _) = through(Some(b"my-gw-run-1")).await;
        assert_eq!(kept, "my-gw-run-1");
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn an_id_settled_by_an_outer_layer_is_kept_by_the_gateways_own() {
        // The access log settles the id outermost and records it in the request's
        // extensions; the gateway's layer must keep that id (a `gw-` id would
        // otherwise be refused as a caller's own) and the response must carry it.
        let app = axum::Router::new()
            .route(
                "/",
                get(|headers: HeaderMap| async move {
                    headers
                        .get("x-request-id")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_owned()
                }),
            )
            .layer(axum::middleware::from_fn(request_id))
            .layer(axum::middleware::from_fn(
                |mut req: Request, next: Next| async move {
                    req.extensions_mut()
                        .insert(SettledRequestId("gw-0123456789ab-7".to_owned()));
                    next.run(req).await
                },
            ));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["x-request-id"], "gw-0123456789ab-7");
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn a_callers_id_is_kept_only_when_it_is_well_formed() {
        let long_enough = "a".repeat(128);
        for kept in ["run-42.step-3:attempt-1", "x", long_enough.as_str()] {
            let (on_response, seen) = through(Some(kept.as_bytes())).await;
            assert_eq!(on_response, kept);
            assert_eq!(seen, kept);
        }
        let too_long = "a".repeat(129);
        for dropped in [
            too_long.as_bytes(),
            b"has a space",
            b"has\ttab",
            b"",
            b"caf\xe9",
        ] {
            let (on_response, seen) = through(Some(dropped)).await;
            assert!(
                on_response.starts_with("gw-"),
                "{dropped:?} -> {on_response}"
            );
            assert_eq!(on_response, seen);
            assert_ne!(on_response.as_bytes(), dropped, "never echoed");
        }
    }

    /// A build without `serve` is unchanged: the process-id form, and whatever header
    /// it was sent.
    #[cfg(not(feature = "serve"))]
    #[tokio::test]
    async fn the_gateway_keeps_its_process_id_form_and_honors_any_header() {
        let (minted, _) = through(None).await;
        let prefix = format!("gw-{}-", std::process::id());
        assert!(minted.starts_with(&prefix), "{minted}");
        let (kept, _) = through(Some(b"has a space")).await;
        assert_eq!(kept, "has a space");
    }

    #[test]
    fn a_restricted_key_must_list_every_model_of_openrouters_own_fallback_list() {
        let key = crate::gateway::auth::Identity {
            tenant: "t".into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
            allowed_models: Some(vec!["openrouter/anthropic/*".into()]),
            key_name: None,
            tenant_budget: None,
        };
        let ask = |payload: Value| {
            request_models_allowed(
                &key,
                "openrouter",
                "openrouter",
                "anthropic/claude-haiku-5-5",
                None,
                &payload,
            )
        };
        assert!(ask(json!({})));
        assert!(ask(
            json!({"x-openrouter": {"models": ["anthropic/claude-sonnet-5"]}})
        ));
        assert!(!ask(
            json!({"x-openrouter": {"models": ["anthropic/a", "openai/gpt-6-sol"]}})
        ));
        // Anything that is not a list of strings is refused rather than guessed at.
        assert!(!ask(
            json!({"x-openrouter": {"models": "openai/gpt-6-sol"}})
        ));
        assert!(!ask(json!({"x-openrouter": {"models": [7]}})));
        // A preset is configured upstream and can set the model there.
        assert!(!ask(
            json!({"x-openrouter": {"preset": "@preset/anything"}})
        ));
        // The namespace is only read by OpenRouter: another provider ignores it.
        let other = request_models_allowed(
            &crate::gateway::auth::Identity {
                allowed_models: Some(vec!["anthropic/*".into()]),
                ..key.clone()
            },
            "anthropic",
            "anthropic",
            "claude-haiku-5-5",
            None,
            &json!({"x-openrouter": {"models": ["openai/gpt-6-sol"]}}),
        );
        assert!(other);
        // A named instance of OpenRouter is still OpenRouter: its fallback list and presets are
        // checked, and the entries are matched under the instance's own name (note 0032).
        let instance_key = crate::gateway::auth::Identity {
            allowed_models: Some(vec!["openrouter-b/anthropic/*".into()]),
            ..key.clone()
        };
        let on_instance = |payload: Value| {
            request_models_allowed(
                &instance_key,
                "openrouter-b",
                "openrouter",
                "anthropic/claude-haiku-5-5",
                None,
                &payload,
            )
        };
        assert!(on_instance(json!({})));
        assert!(on_instance(
            json!({"x-openrouter": {"models": ["anthropic/claude-sonnet-5"]}})
        ));
        assert!(!on_instance(
            json!({"x-openrouter": {"models": ["openai/gpt-6-sol"]}})
        ));
        assert!(!on_instance(
            json!({"x-openrouter": {"preset": "@preset/anything"}})
        ));
    }

    #[test]
    fn only_a_provider_5xx_or_a_transport_failure_is_retried_on_another_target() {
        let upstream = |status: u16, body: &str| {
            GatewayError::Upstream(
                ShimError::ProviderError {
                    status,
                    body: body.into(),
                    retry_after: None,
                }
                .to_string(),
            )
        };
        for status in [500, 502, 503, 504, 529] {
            assert!(
                retryable_upstream_failure(&upstream(status, "boom")),
                "{status}"
            );
        }
        // The request is wrong everywhere, or the failure is not the target's.
        for status in [400, 401, 403, 404, 422, 429] {
            assert!(
                !retryable_upstream_failure(&upstream(status, "no")),
                "{status}"
            );
        }
        // llmshim's own 5xx diagnostics, raised after the provider answered and was billed, would
        // give the same answer on every target: never retried.
        for body in [
            "response did not satisfy the requested output contract",
            "upstream JSON exceeds complexity limit",
            "invalid upstream tool call: bad arguments",
            "invalid upstream response shape",
            "Anthropic response has no supported terminal stop reason",
            "no candidates in response: {}",
            "attempt policy observation failed",
        ] {
            assert!(!retryable_upstream_failure(&upstream(502, body)), "{body}");
            assert!(!retryable_upstream_failure(&upstream(503, body)), "{body}");
        }
        assert!(!retryable_upstream_failure(&upstream(
            502,
            crate::derived_response::DERIVED_RESPONSE_ERROR
        )));
        // Local conditions are never retried elsewhere.
        for message in [
            "llmshim-rate-limit:5000",
            "llmshim-overload:100",
            "llmshim-coordinator-unavailable",
            "llmshim-unpriceable-under-budget",
            "llmshim-provider-unavailable:5000",
            "distributed gateway: Connection reset by peer",
        ] {
            assert!(
                !retryable_upstream_failure(&GatewayError::Upstream(message.into())),
                "{message}"
            );
        }
        assert!(!retryable_upstream_failure(&GatewayError::Timeout));
        assert!(retryable_upstream_failure(&GatewayError::Upstream(
            "HTTP error: error sending request".into()
        )));
    }
}
