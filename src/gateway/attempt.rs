use crate::gateway::auth::Identity;
use crate::policy::{
    AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
    AttemptPolicyErrorKind, AttemptPolicyFuture, AttemptPolicyRefusal, AttemptPolicyRefusalKind,
    DispatchPolicyContext, PreparedAttempt,
};
use crate::proxy::ratelimit::{penalty_duration, RateLimitConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

const ZERO_LIMIT_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TrustedPolicyScope {
    tenant_key: String,
    rpm: Option<u32>,
    tpm: Option<u32>,
}

impl TrustedPolicyScope {
    pub(crate) fn from_identity(identity: &Identity) -> Self {
        Self {
            tenant_key: format!("{:x}", Sha256::digest(identity.tenant.as_bytes())),
            rpm: identity.rpm,
            tpm: identity.tpm,
        }
    }
}

impl std::fmt::Debug for TrustedPolicyScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrustedPolicyScope")
            .field("tenant_key", &"<redacted>")
            .field("rpm", &self.rpm)
            .field("tpm", &self.tpm)
            .finish()
    }
}

#[derive(Clone, Copy)]
enum RateRefusal {
    Provider(Duration),
    Tenant(Duration),
    #[cfg(any(feature = "gateway-redis", test))]
    Unavailable,
}

#[async_trait::async_trait]
trait AttemptRates: Send + Sync {
    async fn acquire(
        &self,
        provider: &str,
        scope: &TrustedPolicyScope,
        permits: u32,
    ) -> Result<(), RateRefusal>;

    async fn penalize(&self, provider: &str, duration: Duration) -> Result<(), ()>;
}

struct Bucket {
    capacity: f64,
    refill_per_second: f64,
    tokens: f64,
    last_refill: Instant,
    penalty_until: Option<Instant>,
}

impl Bucket {
    fn new(per_minute: u32, now: Instant) -> Self {
        let capacity = per_minute as f64;
        Self {
            capacity,
            refill_per_second: capacity / 60.0,
            tokens: capacity,
            last_refill: now,
            penalty_until: None,
        }
    }

    fn check(&mut self, required: f64, now: Instant) -> Result<(), Duration> {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        self.last_refill = now;
        if let Some(deadline) = self.penalty_until {
            if deadline > now {
                return Err(deadline.saturating_duration_since(now));
            }
        }
        if self.tokens + 1e-9 >= required {
            Ok(())
        } else {
            if self.refill_per_second <= 0.0 {
                Err(ZERO_LIMIT_RETRY_AFTER)
            } else {
                Err(Duration::from_secs_f64(
                    (required - self.tokens) / self.refill_per_second,
                ))
            }
        }
    }

    fn debit(&mut self, required: f64) {
        self.tokens -= required;
    }

    fn penalize(&mut self, duration: Duration, now: Instant) {
        let deadline = now + duration;
        self.penalty_until = Some(self.penalty_until.map_or(deadline, |old| old.max(deadline)));
        self.tokens = 0.0;
    }
}

#[derive(Default)]
struct Dimensions {
    rpm: HashMap<String, Bucket>,
    tpm: HashMap<String, Bucket>,
}

struct LocalRateState {
    global: Dimensions,
    tenant: Dimensions,
}

struct LocalAttemptRates {
    config: RateLimitConfig,
    state: tokio::sync::Mutex<LocalRateState>,
}

impl LocalAttemptRates {
    fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            state: tokio::sync::Mutex::new(LocalRateState {
                global: Dimensions::default(),
                tenant: Dimensions::default(),
            }),
        }
    }
}

fn check_dimension(
    buckets: &mut HashMap<String, Bucket>,
    key: &str,
    limit: Option<u32>,
    required: f64,
    now: Instant,
) -> Option<Duration> {
    let limit = limit?;
    if limit == 0 {
        return Some(ZERO_LIMIT_RETRY_AFTER);
    }
    buckets
        .entry(key.to_owned())
        .or_insert_with(|| Bucket::new(limit, now))
        .check(required, now)
        .err()
}

fn debit_dimension(
    buckets: &mut HashMap<String, Bucket>,
    key: &str,
    limit: Option<u32>,
    required: f64,
) {
    if limit.is_some() {
        buckets
            .get_mut(key)
            .expect("checked bucket must exist")
            .debit(required);
    }
}

#[async_trait::async_trait]
impl AttemptRates for LocalAttemptRates {
    async fn acquire(
        &self,
        provider: &str,
        scope: &TrustedPolicyScope,
        permits: u32,
    ) -> Result<(), RateRefusal> {
        let global_limit = self.config.resolve(provider);
        let now = Instant::now();
        let tenant_key = format!("{}:{provider}", scope.tenant_key);
        let mut state = self.state.lock().await;
        let mut provider_wait = None;
        let mut tenant_wait = None;
        for wait in [
            check_dimension(&mut state.global.rpm, provider, global_limit.rpm, 1.0, now),
            check_dimension(
                &mut state.global.tpm,
                provider,
                global_limit.tpm,
                permits.max(1) as f64,
                now,
            ),
        ]
        .into_iter()
        .flatten()
        {
            provider_wait = Some(provider_wait.map_or(wait, |current: Duration| current.max(wait)));
        }
        for wait in [
            check_dimension(&mut state.tenant.rpm, &tenant_key, scope.rpm, 1.0, now),
            check_dimension(
                &mut state.tenant.tpm,
                &tenant_key,
                scope.tpm,
                permits.max(1) as f64,
                now,
            ),
        ]
        .into_iter()
        .flatten()
        {
            tenant_wait = Some(tenant_wait.map_or(wait, |current: Duration| current.max(wait)));
        }

        if let Some(wait) = tenant_wait {
            return Err(RateRefusal::Tenant(wait));
        }
        if let Some(wait) = provider_wait {
            return Err(RateRefusal::Provider(wait));
        }

        debit_dimension(&mut state.global.rpm, provider, global_limit.rpm, 1.0);
        debit_dimension(
            &mut state.global.tpm,
            provider,
            global_limit.tpm,
            permits.max(1) as f64,
        );
        debit_dimension(&mut state.tenant.rpm, &tenant_key, scope.rpm, 1.0);
        debit_dimension(
            &mut state.tenant.tpm,
            &tenant_key,
            scope.tpm,
            permits.max(1) as f64,
        );
        Ok(())
    }

    async fn penalize(&self, provider: &str, duration: Duration) -> Result<(), ()> {
        let limit = self.config.resolve(provider);
        let now = Instant::now();
        let mut state = self.state.lock().await;
        if let Some(rpm) = limit.rpm.filter(|limit| *limit > 0) {
            state
                .global
                .rpm
                .entry(provider.to_owned())
                .or_insert_with(|| Bucket::new(rpm, now))
                .penalize(duration, now);
        }
        if let Some(tpm) = limit.tpm.filter(|limit| *limit > 0) {
            state
                .global
                .tpm
                .entry(provider.to_owned())
                .or_insert_with(|| Bucket::new(tpm, now))
                .penalize(duration, now);
        }
        Ok(())
    }
}

pub(crate) struct AttemptCoordinator {
    rates: Arc<dyn AttemptRates>,
    concurrency_limit: usize,
    concurrency_wait: Duration,
    semaphores: Mutex<HashMap<String, Arc<Semaphore>>>,
    active_permits: Mutex<HashMap<uuid::Uuid, OwnedSemaphorePermit>>,
}

impl AttemptCoordinator {
    pub(crate) fn local(
        config: RateLimitConfig,
        concurrency_limit: usize,
        concurrency_wait: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            rates: Arc::new(LocalAttemptRates::new(config)),
            concurrency_limit: concurrency_limit.max(1),
            concurrency_wait,
            semaphores: Mutex::new(HashMap::new()),
            active_permits: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(feature = "redis-coordination")]
    pub(crate) fn redis(
        redis_url: &str,
        config: RateLimitConfig,
        concurrency_limit: usize,
        concurrency_wait: Duration,
    ) -> redis::RedisResult<Arc<Self>> {
        Ok(Arc::new(Self {
            rates: Arc::new(RedisAttemptRates::new(redis_url, config)?),
            concurrency_limit: concurrency_limit.max(1),
            concurrency_wait,
            semaphores: Mutex::new(HashMap::new()),
            active_permits: Mutex::new(HashMap::new()),
        }))
    }

    pub(crate) fn context(self: &Arc<Self>, scope: TrustedPolicyScope) -> DispatchPolicyContext {
        DispatchPolicyContext::new(Arc::new(CoordinatedAttemptPolicy {
            coordinator: self.clone(),
            scope,
        }))
    }

    #[cfg(test)]
    pub(crate) fn unavailable_for_test(
        concurrency_limit: usize,
        concurrency_wait: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            rates: Arc::new(UnavailableAttemptRates),
            concurrency_limit: concurrency_limit.max(1),
            concurrency_wait,
            semaphores: Mutex::new(HashMap::new()),
            active_permits: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    pub(crate) async fn hold_provider_for_test(&self, provider: &str) -> OwnedSemaphorePermit {
        let semaphore = {
            let mut semaphores = self.semaphores.lock().unwrap();
            semaphores
                .entry(provider.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(self.concurrency_limit)))
                .clone()
        };
        semaphore.acquire_owned().await.unwrap()
    }

    async fn acquire(
        &self,
        attempt: &PreparedAttempt<'_>,
        scope: &TrustedPolicyScope,
    ) -> Result<(), AttemptPolicyRefusal> {
        let provider = attempt.identity().provider_name();
        let semaphore = {
            let mut semaphores = self.semaphores.lock().unwrap();
            semaphores
                .entry(provider.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(self.concurrency_limit)))
                .clone()
        };
        let permit = tokio::time::timeout(self.concurrency_wait, semaphore.acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                AttemptPolicyRefusal::new(
                    AttemptPolicyRefusalKind::CoordinatorUnavailable,
                    Some(self.concurrency_wait),
                )
            })?;
        let permits = crate::proxy::ratelimit::estimate_attempt_tokens(attempt);
        match self.rates.acquire(provider, scope, permits).await {
            Ok(()) => {
                self.active_permits
                    .lock()
                    .unwrap()
                    .insert(attempt.identity().id(), permit);
                Ok(())
            }
            Err(RateRefusal::Provider(wait)) => Err(AttemptPolicyRefusal::new(
                AttemptPolicyRefusalKind::ProviderLimit,
                Some(wait),
            )),
            Err(RateRefusal::Tenant(wait)) => Err(AttemptPolicyRefusal::new(
                AttemptPolicyRefusalKind::TenantLimit,
                Some(wait),
            )),
            #[cfg(any(feature = "gateway-redis", test))]
            Err(RateRefusal::Unavailable) => Err(AttemptPolicyRefusal::new(
                AttemptPolicyRefusalKind::CoordinatorUnavailable,
                None,
            )),
        }
    }

    fn release(&self, attempt_id: uuid::Uuid) {
        self.active_permits.lock().unwrap().remove(&attempt_id);
    }
}

#[cfg(test)]
struct UnavailableAttemptRates;

#[cfg(test)]
#[async_trait::async_trait]
impl AttemptRates for UnavailableAttemptRates {
    async fn acquire(
        &self,
        _provider: &str,
        _scope: &TrustedPolicyScope,
        _permits: u32,
    ) -> Result<(), RateRefusal> {
        Err(RateRefusal::Unavailable)
    }

    async fn penalize(&self, _provider: &str, _duration: Duration) -> Result<(), ()> {
        Err(())
    }
}

struct CoordinatedAttemptPolicy {
    coordinator: Arc<AttemptCoordinator>,
    scope: TrustedPolicyScope,
}

impl AttemptPolicy for CoordinatedAttemptPolicy {
    fn acquire<'a>(
        &'a self,
        attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async move { self.coordinator.acquire(attempt, &self.scope).await })
    }

    fn observe<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            match event {
                AttemptEvent::ResponseHeaders { status: 429 } => self
                    .coordinator
                    .rates
                    .penalize(attempt.provider_name(), penalty_duration())
                    .await
                    .map_err(|_| {
                        AttemptPolicyError::new(AttemptPolicyErrorKind::CoordinatorUnavailable)
                    }),
                AttemptEvent::Finished(_) => {
                    self.coordinator.release(attempt.id());
                    Ok(())
                }
                AttemptEvent::ResponseHeaders { .. } | AttemptEvent::Usage { .. } => Ok(()),
            }
        })
    }

    fn observe_abandoned(
        &self,
        attempt: &AttemptIdentity,
        _outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        self.coordinator.release(attempt.id());
        Ok(())
    }
}

#[cfg(feature = "redis-coordination")]
mod redis_rates {
    use super::*;
    use redis::aio::ConnectionManager;
    use tokio::sync::OnceCell;

    const KEY_TTL_MS: u64 = 3_600_000;
    const COMBINED_RATE_LUA: &str = r#"
        local now = tonumber(ARGV[1])
        local ttl = tonumber(ARGV[2])
        local admitted = 1
        local wait = 0
        local refusal = 0
        local balances = {}
        local timestamps = {}
        local penalties = {}

        for dimension = 1, 4 do
            local offset = 3 + (dimension - 1) * 4
            local enabled = tonumber(ARGV[offset]) == 1
            if enabled then
                local capacity = tonumber(ARGV[offset + 1])
                local refill = tonumber(ARGV[offset + 2])
                local required = tonumber(ARGV[offset + 3])
                local stored = redis.call('HMGET', KEYS[dimension], 'tokens', 'ts', 'penalty')
                local balance = tonumber(stored[1])
                local timestamp = tonumber(stored[2])
                local penalty = tonumber(stored[3]) or 0
                if balance == nil then balance = capacity end
                if timestamp == nil then timestamp = now end
                local elapsed = now - timestamp
                if elapsed < 0 then elapsed = 0 end
                balance = math.min(capacity, balance + (elapsed / 1000.0) * refill)
                local dimension_wait = 0
                if penalty > now then
                    dimension_wait = penalty - now
                elseif balance < required then
                    dimension_wait = math.ceil((required - balance) / refill * 1000.0)
                end
                if dimension_wait > 0 then
                    admitted = 0
                    wait = math.max(wait, dimension_wait)
                    if dimension >= 3 then refusal = 2 elseif refusal == 0 then refusal = 1 end
                end
                balances[dimension] = balance
                timestamps[dimension] = now
                penalties[dimension] = penalty
            end
        end

        if admitted == 1 then
            for dimension = 1, 4 do
                local offset = 3 + (dimension - 1) * 4
                if tonumber(ARGV[offset]) == 1 then
                    balances[dimension] = balances[dimension] - tonumber(ARGV[offset + 3])
                end
            end
        end

        for dimension = 1, 4 do
            local offset = 3 + (dimension - 1) * 4
            if tonumber(ARGV[offset]) == 1 then
                redis.call('HSET', KEYS[dimension],
                    'tokens', balances[dimension],
                    'ts', timestamps[dimension],
                    'penalty', penalties[dimension])
                redis.call('PEXPIRE', KEYS[dimension], ttl)
            end
        end
        return {admitted, wait, refusal}
    "#;

    const PENALTY_LUA: &str = r#"
        local deadline = tonumber(ARGV[1])
        local ttl = tonumber(ARGV[2])
        for index = 1, 2 do
            redis.call('HSET', KEYS[index], 'penalty', deadline, 'tokens', 0)
            redis.call('PEXPIRE', KEYS[index], ttl)
        end
        return 1
    "#;

    pub(super) struct RedisAttemptRates {
        client: redis::Client,
        connection: OnceCell<ConnectionManager>,
        config: RateLimitConfig,
        combined_script: redis::Script,
        penalty_script: redis::Script,
    }

    impl RedisAttemptRates {
        pub(super) fn new(url: &str, config: RateLimitConfig) -> redis::RedisResult<Self> {
            Ok(Self {
                client: redis::Client::open(url)?,
                connection: OnceCell::new(),
                config,
                combined_script: redis::Script::new(COMBINED_RATE_LUA),
                penalty_script: redis::Script::new(PENALTY_LUA),
            })
        }

        async fn connection(&self) -> redis::RedisResult<ConnectionManager> {
            self.connection
                .get_or_try_init(|| ConnectionManager::new(self.client.clone()))
                .await
                .cloned()
        }

        fn global_key(provider: &str, dimension: &str) -> String {
            format!("llmshim:rl:{provider}:{dimension}")
        }

        fn tenant_key(scope: &TrustedPolicyScope, provider: &str, dimension: &str) -> String {
            format!(
                "llmshim:rl:tenant:{}:{provider}:{dimension}",
                scope.tenant_key
            )
        }

        fn add_dimension(
            invocation: &mut redis::ScriptInvocation<'_>,
            limit: Option<u32>,
            required: f64,
        ) {
            let per_minute = limit.unwrap_or(1).max(1) as f64;
            invocation
                .arg(i32::from(limit.is_some()))
                .arg(per_minute)
                .arg(per_minute / 60.0)
                .arg(required);
        }
    }

    #[async_trait::async_trait]
    impl AttemptRates for RedisAttemptRates {
        async fn acquire(
            &self,
            provider: &str,
            scope: &TrustedPolicyScope,
            permits: u32,
        ) -> Result<(), RateRefusal> {
            let global = self.config.resolve(provider);
            if scope.rpm == Some(0) || scope.tpm == Some(0) {
                return Err(RateRefusal::Tenant(ZERO_LIMIT_RETRY_AFTER));
            }
            if global.rpm == Some(0) || global.tpm == Some(0) {
                return Err(RateRefusal::Provider(ZERO_LIMIT_RETRY_AFTER));
            }
            let mut connection = self
                .connection()
                .await
                .map_err(|_| RateRefusal::Unavailable)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            let mut invocation = self.combined_script.prepare_invoke();
            invocation
                .key(Self::global_key(provider, "rpm"))
                .key(Self::global_key(provider, "tpm"))
                .key(Self::tenant_key(scope, provider, "rpm"))
                .key(Self::tenant_key(scope, provider, "tpm"))
                .arg(now)
                .arg(KEY_TTL_MS);
            Self::add_dimension(&mut invocation, global.rpm, 1.0);
            Self::add_dimension(&mut invocation, global.tpm, permits.max(1) as f64);
            Self::add_dimension(&mut invocation, scope.rpm, 1.0);
            Self::add_dimension(&mut invocation, scope.tpm, permits.max(1) as f64);
            let (admitted, wait_ms, refusal): (i64, i64, i64) = invocation
                .invoke_async(&mut connection)
                .await
                .map_err(|_| RateRefusal::Unavailable)?;
            if admitted == 1 {
                Ok(())
            } else if refusal == 2 {
                Err(RateRefusal::Tenant(Duration::from_millis(
                    wait_ms.max(0) as u64
                )))
            } else {
                Err(RateRefusal::Provider(Duration::from_millis(
                    wait_ms.max(0) as u64
                )))
            }
        }

        async fn penalize(&self, provider: &str, duration: Duration) -> Result<(), ()> {
            let limit = self.config.resolve(provider);
            if limit.rpm.is_none() && limit.tpm.is_none() {
                return Ok(());
            }
            let mut connection = self.connection().await.map_err(|_| ())?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_millis() as u64)
                .unwrap_or_default();
            let _: i64 = self
                .penalty_script
                .key(Self::global_key(provider, "rpm"))
                .key(Self::global_key(provider, "tpm"))
                .arg(now.saturating_add(duration.as_millis() as u64))
                .arg(KEY_TTL_MS)
                .invoke_async(&mut connection)
                .await
                .map_err(|_| ())?;
            Ok(())
        }
    }
}

#[cfg(feature = "redis-coordination")]
use redis_rates::RedisAttemptRates;

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(name: &str, rpm: Option<u32>, tpm: Option<u32>) -> TrustedPolicyScope {
        TrustedPolicyScope {
            tenant_key: format!("test-{name}"),
            rpm,
            tpm,
        }
    }

    #[tokio::test]
    async fn local_provider_and_tenant_dimensions_are_each_enforced() {
        let provider_rates = LocalAttemptRates::new(RateLimitConfig::with_global(Some(1), None));
        let unlimited_tenant = scope("provider", None, None);
        assert!(provider_rates
            .acquire("provider", &unlimited_tenant, 1)
            .await
            .is_ok());
        assert!(matches!(
            provider_rates
                .acquire("provider", &unlimited_tenant, 1)
                .await,
            Err(RateRefusal::Provider(_))
        ));

        let tenant_rates = LocalAttemptRates::new(RateLimitConfig::default());
        let limited_tenant = scope("tenant", Some(1), None);
        assert!(tenant_rates
            .acquire("provider", &limited_tenant, 1)
            .await
            .is_ok());
        assert!(matches!(
            tenant_rates.acquire("provider", &limited_tenant, 1).await,
            Err(RateRefusal::Tenant(_))
        ));
    }

    #[tokio::test]
    async fn local_tenant_denial_does_not_consume_global_allowance() {
        let rates = LocalAttemptRates::new(RateLimitConfig::with_global(Some(1), None));
        let denied_tenant = scope("denied", None, Some(1));
        assert!(matches!(
            rates.acquire("provider", &denied_tenant, 2).await,
            Err(RateRefusal::Tenant(_))
        ));
        assert!(rates
            .acquire("provider", &scope("allowed", None, None), 1)
            .await
            .is_ok());
    }

    #[cfg(feature = "redis-coordination")]
    fn redis_url() -> Option<String> {
        std::env::var("LLMSHIM_REDIS_URL").ok()
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_tenant_allowance_is_shared_across_coordinators() {
        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-share-{}", uuid::Uuid::new_v4().simple());
        let config = RateLimitConfig::with_global(Some(100), None);
        let first = RedisAttemptRates::new(&redis_url, config.clone()).unwrap();
        let second = RedisAttemptRates::new(&redis_url, config).unwrap();
        let tenant = scope("shared", Some(1), None);
        assert!(first.acquire(&provider, &tenant, 1).await.is_ok());
        assert!(matches!(
            second.acquire(&provider, &tenant, 1).await,
            Err(RateRefusal::Tenant(_))
        ));
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_tenant_denial_is_atomic_with_global_dimensions() {
        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-atomic-{}", uuid::Uuid::new_v4().simple());
        let rates = RedisAttemptRates::new(&redis_url, RateLimitConfig::with_global(Some(1), None))
            .unwrap();
        assert!(matches!(
            rates
                .acquire(&provider, &scope("denied", None, Some(1)), 2)
                .await,
            Err(RateRefusal::Tenant(_))
        ));
        assert!(rates
            .acquire(&provider, &scope("allowed", None, None), 1)
            .await
            .is_ok());
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_global_bucket_is_shared_with_the_legacy_limiter() {
        use crate::proxy::ratelimit::{RateKey, RateLimiter, RedisRateLimiter};

        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-compatible-{}", uuid::Uuid::new_v4().simple());
        let config = RateLimitConfig::with_global(Some(1), None);
        let coordinated = RedisAttemptRates::new(&redis_url, config.clone()).unwrap();
        let legacy = RedisRateLimiter::new(&redis_url, config).unwrap();
        assert!(coordinated
            .acquire(&provider, &scope("tenant", None, None), 1)
            .await
            .is_ok());
        assert!(legacy
            .acquire(&RateKey::provider(provider), 1)
            .await
            .is_err());
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_redelivery_acquisition_debits_a_new_attempt() {
        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-redelivery-{}", uuid::Uuid::new_v4().simple());
        let rates =
            RedisAttemptRates::new(&redis_url, RateLimitConfig::with_global(Some(100), None))
                .unwrap();
        let tenant = scope("redelivered-job", Some(2), None);

        assert!(rates.acquire(&provider, &tenant, 1).await.is_ok());
        assert!(rates.acquire(&provider, &tenant, 1).await.is_ok());
        assert!(matches!(
            rates.acquire(&provider, &tenant, 1).await,
            Err(RateRefusal::Tenant(_))
        ));
    }
}
