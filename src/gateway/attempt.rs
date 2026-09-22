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
    #[serde(default)]
    version: u8,
    tenant_key: String,
    rpm: Option<u32>,
    tpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget: Option<crate::gateway::budget::BudgetPolicySnapshot>,
}

impl TrustedPolicyScope {
    pub(crate) fn from_identity(identity: &Identity) -> Self {
        Self {
            version: 1,
            tenant_key: format!("{:x}", Sha256::digest(identity.tenant.as_bytes())),
            rpm: identity.rpm,
            tpm: identity.tpm,
            budget: crate::gateway::budget::BudgetPolicySnapshot::from_identity(identity),
        }
    }
}

impl std::fmt::Debug for TrustedPolicyScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrustedPolicyScope")
            .field("version", &self.version)
            .field("tenant_key", &"<redacted>")
            .field("rpm", &self.rpm)
            .field("tpm", &self.tpm)
            .field("budget", &self.budget)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
enum RateRefusal {
    Provider(Duration),
    Tenant(Duration),
    Budget(Duration),
    Unavailable,
}

#[async_trait::async_trait]
trait AttemptRates: Send + Sync {
    async fn acquire(
        &self,
        provider: &str,
        scope: &TrustedPolicyScope,
        permits: u32,
        attempt_id: uuid::Uuid,
        reservation: Option<crate::gateway::budget::BudgetReservation>,
    ) -> Result<(), RateRefusal>;

    async fn penalize(&self, provider: &str, duration: Duration) -> Result<(), ()>;

    async fn observe_usage(
        &self,
        scope: &TrustedPolicyScope,
        attempt_id: uuid::Uuid,
        cost_nanos: Option<u64>,
    ) -> Result<(), ()>;

    async fn finish(
        &self,
        scope: &TrustedPolicyScope,
        attempt_id: uuid::Uuid,
        outcome: AttemptOutcome,
    ) -> Result<(), ()>;

    #[cfg(test)]
    async fn budget_total(&self, scope: &TrustedPolicyScope) -> Option<u64>;
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
    budget_totals: HashMap<BudgetWindowKey, u64>,
    budget_attempts: HashMap<uuid::Uuid, AttemptLiability>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct BudgetWindowKey {
    tenant_key: String,
    window_secs: u64,
    window_index: u64,
}

struct AttemptLiability {
    window_key: BudgetWindowKey,
    liability_nanos: u64,
    observed_nanos: Option<u64>,
    finalized: bool,
    expires_at_epoch_secs: u64,
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
                budget_totals: HashMap::new(),
                budget_attempts: HashMap::new(),
            }),
        }
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn budget_wait(window_secs: u64, now_epoch_secs: u64) -> Duration {
    Duration::from_secs(window_secs - (now_epoch_secs % window_secs))
}

fn purge_expired_budget_state(state: &mut LocalRateState, now_epoch_secs: u64) {
    state
        .budget_attempts
        .retain(|_, attempt| attempt.expires_at_epoch_secs > now_epoch_secs);
    let active_windows: std::collections::HashSet<_> = state
        .budget_attempts
        .values()
        .map(|attempt| attempt.window_key.clone())
        .collect();
    state
        .budget_totals
        .retain(|window, _| active_windows.contains(window));
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
        attempt_id: uuid::Uuid,
        reservation: Option<crate::gateway::budget::BudgetReservation>,
    ) -> Result<(), RateRefusal> {
        let global_limit = self.config.resolve(provider);
        let now = Instant::now();
        let tenant_key = format!("{}:{provider}", scope.tenant_key);
        let mut state = self.state.lock().await;
        let now_epoch_secs = epoch_secs();
        purge_expired_budget_state(&mut state, now_epoch_secs);
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

        let budget_commit = match (&scope.budget, reservation) {
            (None, None) => None,
            (Some(policy), Some(mut reservation)) => {
                if state.budget_attempts.contains_key(&attempt_id) {
                    return Err(RateRefusal::Unavailable);
                }
                let window_key = BudgetWindowKey {
                    tenant_key: scope.tenant_key.clone(),
                    window_secs: policy.window_secs,
                    window_index: reservation.window_index,
                };
                let current = state.budget_totals.get(&window_key).copied().unwrap_or(0);
                let admitted = if reservation.bounded {
                    current
                        .checked_add(reservation.amount_nanos)
                        .is_some_and(|next| next <= policy.limit_nanos)
                } else {
                    current < policy.limit_nanos
                };
                if !admitted {
                    return Err(RateRefusal::Budget(budget_wait(
                        policy.window_secs,
                        now_epoch_secs,
                    )));
                }
                if !reservation.bounded {
                    reservation.amount_nanos = policy.limit_nanos - current;
                }
                Some((window_key, reservation))
            }
            _ => return Err(RateRefusal::Unavailable),
        };

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
        if let Some((window_key, reservation)) = budget_commit {
            let total = state.budget_totals.entry(window_key.clone()).or_insert(0);
            *total = total
                .checked_add(reservation.amount_nanos)
                .ok_or(RateRefusal::Unavailable)?;
            state.budget_attempts.insert(
                attempt_id,
                AttemptLiability {
                    window_key,
                    liability_nanos: reservation.amount_nanos,
                    observed_nanos: None,
                    finalized: false,
                    expires_at_epoch_secs: reservation.expires_at_epoch_secs,
                },
            );
        }
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

    async fn observe_usage(
        &self,
        scope: &TrustedPolicyScope,
        attempt_id: uuid::Uuid,
        cost_nanos: Option<u64>,
    ) -> Result<(), ()> {
        if scope.budget.is_none() {
            return Ok(());
        }
        let Some(cost_nanos) = cost_nanos else {
            return Ok(());
        };
        let mut state = self.state.lock().await;
        let increase = {
            let attempt = state.budget_attempts.get_mut(&attempt_id).ok_or(())?;
            if attempt.finalized {
                return Ok(());
            }
            let old_observed = attempt.observed_nanos.unwrap_or(0);
            if cost_nanos <= old_observed {
                return Ok(());
            }
            attempt.observed_nanos = Some(cost_nanos);
            if cost_nanos > attempt.liability_nanos {
                let delta = cost_nanos - attempt.liability_nanos;
                attempt.liability_nanos = cost_nanos;
                Some((attempt.window_key.clone(), delta))
            } else {
                None
            }
        };
        if let Some((window_key, delta)) = increase {
            let total = state.budget_totals.get_mut(&window_key).ok_or(())?;
            *total = total.checked_add(delta).ok_or(())?;
        }
        Ok(())
    }

    async fn finish(
        &self,
        scope: &TrustedPolicyScope,
        attempt_id: uuid::Uuid,
        outcome: AttemptOutcome,
    ) -> Result<(), ()> {
        if scope.budget.is_none() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let release = {
            let attempt = state.budget_attempts.get_mut(&attempt_id).ok_or(())?;
            if attempt.finalized {
                return Ok(());
            }
            if crate::gateway::budget::may_finalize(outcome) {
                if let Some(observed_nanos) = attempt.observed_nanos {
                    let release = if observed_nanos < attempt.liability_nanos {
                        let released = attempt.liability_nanos - observed_nanos;
                        attempt.liability_nanos = observed_nanos;
                        Some((attempt.window_key.clone(), released))
                    } else {
                        None
                    };
                    attempt.finalized = true;
                    release
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some((window_key, released)) = release {
            let total = state.budget_totals.get_mut(&window_key).ok_or(())?;
            *total = total.checked_sub(released).ok_or(())?;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn budget_total(&self, scope: &TrustedPolicyScope) -> Option<u64> {
        let policy = scope.budget.as_ref()?;
        let index = epoch_secs() / policy.window_secs;
        let state = self.state.lock().await;
        Some(total_for_scope(&state, scope, policy.window_secs, index))
    }
}

#[cfg(test)]
fn total_for_scope(
    state: &LocalRateState,
    scope: &TrustedPolicyScope,
    window_secs: u64,
    window_index: u64,
) -> u64 {
    state
        .budget_totals
        .get(&BudgetWindowKey {
            tenant_key: scope.tenant_key.clone(),
            window_secs,
            window_index,
        })
        .copied()
        .unwrap_or_default()
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

    #[cfg(test)]
    pub(crate) async fn budget_total_for_test(&self, scope: &TrustedPolicyScope) -> Option<u64> {
        self.rates.budget_total(scope).await
    }

    async fn acquire(
        &self,
        attempt: &PreparedAttempt<'_>,
        scope: &TrustedPolicyScope,
    ) -> Result<(), AttemptPolicyRefusal> {
        if scope.version != 1 {
            return Err(AttemptPolicyRefusal::new(
                AttemptPolicyRefusalKind::CoordinatorUnavailable,
                None,
            ));
        }
        let reservation =
            crate::gateway::budget::reservation(scope.budget.as_ref(), attempt, epoch_secs())
                .map_err(|_| {
                    AttemptPolicyRefusal::new(AttemptPolicyRefusalKind::Unpriceable, None)
                })?;
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
        match self
            .rates
            .acquire(
                provider,
                scope,
                permits,
                attempt.identity().id(),
                reservation,
            )
            .await
        {
            Ok(()) => {
                if reservation.is_some_and(|reservation| !reservation.bounded) {
                    crate::gateway::metrics::incr(
                        crate::gateway::metrics::UNPRICED_UNDER_CAP,
                        &[
                            ("provider", provider),
                            ("model", attempt.identity().native_model()),
                        ],
                    );
                }
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
            Err(RateRefusal::Budget(wait)) => Err(AttemptPolicyRefusal::new(
                AttemptPolicyRefusalKind::Budget,
                Some(wait),
            )),
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
        _attempt_id: uuid::Uuid,
        _reservation: Option<crate::gateway::budget::BudgetReservation>,
    ) -> Result<(), RateRefusal> {
        Err(RateRefusal::Unavailable)
    }

    async fn penalize(&self, _provider: &str, _duration: Duration) -> Result<(), ()> {
        Err(())
    }

    async fn observe_usage(
        &self,
        _scope: &TrustedPolicyScope,
        _attempt_id: uuid::Uuid,
        _cost_nanos: Option<u64>,
    ) -> Result<(), ()> {
        Err(())
    }

    async fn finish(
        &self,
        _scope: &TrustedPolicyScope,
        _attempt_id: uuid::Uuid,
        _outcome: AttemptOutcome,
    ) -> Result<(), ()> {
        Err(())
    }

    async fn budget_total(&self, _scope: &TrustedPolicyScope) -> Option<u64> {
        None
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
                AttemptEvent::Finished(outcome) => {
                    self.coordinator
                        .rates
                        .finish(&self.scope, attempt.id(), outcome)
                        .await
                        .map_err(|_| {
                            AttemptPolicyError::new(AttemptPolicyErrorKind::CoordinatorUnavailable)
                        })?;
                    self.coordinator.release(attempt.id());
                    Ok(())
                }
                AttemptEvent::Usage { usage } => self
                    .coordinator
                    .rates
                    .observe_usage(
                        &self.scope,
                        attempt.id(),
                        crate::gateway::budget::observed_cost_nanos(usage),
                    )
                    .await
                    .map_err(|_| {
                        AttemptPolicyError::new(AttemptPolicyErrorKind::CoordinatorUnavailable)
                    }),
                AttemptEvent::ResponseHeaders { .. } => Ok(()),
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

        local budget_enabled = tonumber(ARGV[19]) == 1
        local budget_next = 0
        local reservation = 0
        if budget_enabled then
            local bounded = tonumber(ARGV[20]) == 1
            local budget_limit = tonumber(ARGV[21])
            reservation = tonumber(ARGV[22])
            local current = tonumber(redis.call('GET', KEYS[5])) or 0
            if redis.call('EXISTS', KEYS[6]) == 1 then
                admitted = 0
                refusal = 4
            elseif bounded then
                if reservation > budget_limit or current > budget_limit - reservation then
                    admitted = 0
                    refusal = 3
                else
                    budget_next = current + reservation
                end
            elseif current >= budget_limit then
                admitted = 0
                refusal = 3
            else
                reservation = budget_limit - current
                budget_next = budget_limit
            end
        end

        if admitted == 1 then
            for dimension = 1, 4 do
                local offset = 3 + (dimension - 1) * 4
                if tonumber(ARGV[offset]) == 1 then
                    balances[dimension] = balances[dimension] - tonumber(ARGV[offset + 3])
                end
            end
            if budget_enabled then
                redis.call('SET', KEYS[5], budget_next, 'PX', ARGV[23])
                redis.call('HSET', KEYS[6],
                    'liability', reservation,
                    'observed', -1,
                    'finalized', 0,
                    'total_key', KEYS[5])
                redis.call('PEXPIRE', KEYS[6], ARGV[23])
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

    const SETTLE_BUDGET_LUA: &str = r#"
        if redis.call('EXISTS', KEYS[1]) == 0 then return -1 end
        local values = redis.call('HMGET', KEYS[1], 'liability', 'observed', 'finalized', 'total_key')
        local liability = tonumber(values[1]) or 0
        local observed = tonumber(values[2]) or -1
        local finalized = tonumber(values[3]) or 0
        local total_key = values[4]
        if finalized == 1 then return 1 end
        local incoming = tonumber(ARGV[1])
        local should_finalize = tonumber(ARGV[2]) == 1
        local max_exact = tonumber(ARGV[3])
        if incoming >= 0 and incoming > observed then
            observed = incoming
            redis.call('HSET', KEYS[1], 'observed', observed)
        end
        if observed > liability then
            local total = tonumber(redis.call('GET', total_key)) or 0
            local delta = observed - liability
            if total > max_exact - delta then
                total = max_exact
                liability = max_exact
            else
                total = total + delta
                liability = observed
            end
            redis.call('SET', total_key, total, 'KEEPTTL')
            redis.call('HSET', KEYS[1], 'liability', liability)
        end
        if should_finalize and observed >= 0 then
            if observed < liability then
                local total = tonumber(redis.call('GET', total_key)) or 0
                total = math.max(0, total - (liability - observed))
                redis.call('SET', total_key, total, 'KEEPTTL')
                liability = observed
                redis.call('HSET', KEYS[1], 'liability', liability)
            end
            redis.call('HSET', KEYS[1], 'finalized', 1)
        end
        return 1
    "#;

    pub(super) struct RedisAttemptRates {
        client: redis::Client,
        connection: OnceCell<ConnectionManager>,
        config: RateLimitConfig,
        combined_script: redis::Script,
        penalty_script: redis::Script,
        settle_budget_script: redis::Script,
    }

    impl RedisAttemptRates {
        pub(super) fn new(url: &str, config: RateLimitConfig) -> redis::RedisResult<Self> {
            Ok(Self {
                client: redis::Client::open(url)?,
                connection: OnceCell::new(),
                config,
                combined_script: redis::Script::new(COMBINED_RATE_LUA),
                penalty_script: redis::Script::new(PENALTY_LUA),
                settle_budget_script: redis::Script::new(SETTLE_BUDGET_LUA),
            })
        }

        pub(super) async fn connection(&self) -> redis::RedisResult<ConnectionManager> {
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

        pub(super) fn budget_total_key(scope: &TrustedPolicyScope, window_index: u64) -> String {
            format!(
                "llmshim:gw:budget:v1:{}:{}:{window_index}",
                scope.tenant_key,
                scope.budget.as_ref().map_or(0, |budget| budget.window_secs)
            )
        }

        pub(super) fn budget_attempt_key(
            scope: &TrustedPolicyScope,
            attempt_id: uuid::Uuid,
        ) -> String {
            format!(
                "llmshim:gw:budget:v1:attempt:{}:{attempt_id}",
                scope.tenant_key
            )
        }

        async fn settle_budget(
            &self,
            scope: &TrustedPolicyScope,
            attempt_id: uuid::Uuid,
            cost_nanos: Option<u64>,
            finalize: bool,
        ) -> Result<(), ()> {
            if scope.budget.is_none() {
                return Ok(());
            }
            let mut connection = self.connection().await.map_err(|_| ())?;
            let result: i64 = self
                .settle_budget_script
                .key(Self::budget_attempt_key(scope, attempt_id))
                .arg(cost_nanos.map_or(-1_i64, |value| value as i64))
                .arg(i32::from(finalize))
                .arg(crate::gateway::budget::MAX_EXACT_REDIS_NANOS)
                .invoke_async(&mut connection)
                .await
                .map_err(|_| ())?;
            (result == 1).then_some(()).ok_or(())
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
            attempt_id: uuid::Uuid,
            reservation: Option<crate::gateway::budget::BudgetReservation>,
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
                .key(Self::tenant_key(scope, provider, "tpm"));
            match (&scope.budget, reservation) {
                (Some(_), Some(reservation)) => {
                    invocation
                        .key(Self::budget_total_key(scope, reservation.window_index))
                        .key(Self::budget_attempt_key(scope, attempt_id));
                }
                (None, None) => {
                    invocation
                        .key("llmshim:gw:budget:v1:none")
                        .key(format!("llmshim:gw:budget:v1:none:{attempt_id}"));
                }
                _ => return Err(RateRefusal::Unavailable),
            }
            invocation.arg(now).arg(KEY_TTL_MS);
            Self::add_dimension(&mut invocation, global.rpm, 1.0);
            Self::add_dimension(&mut invocation, global.tpm, permits.max(1) as f64);
            Self::add_dimension(&mut invocation, scope.rpm, 1.0);
            Self::add_dimension(&mut invocation, scope.tpm, permits.max(1) as f64);
            match (&scope.budget, reservation) {
                (Some(policy), Some(reservation)) => {
                    let ttl_ms = reservation
                        .expires_at_epoch_secs
                        .saturating_sub(now / 1000)
                        .saturating_mul(1000)
                        .max(1);
                    invocation
                        .arg(1)
                        .arg(i32::from(reservation.bounded))
                        .arg(policy.limit_nanos)
                        .arg(reservation.amount_nanos)
                        .arg(ttl_ms);
                }
                (None, None) => {
                    invocation.arg(0).arg(0).arg(0).arg(0).arg(1);
                }
                _ => return Err(RateRefusal::Unavailable),
            }
            let (admitted, wait_ms, refusal): (i64, i64, i64) = invocation
                .invoke_async(&mut connection)
                .await
                .map_err(|_| RateRefusal::Unavailable)?;
            if admitted == 1 {
                Ok(())
            } else if refusal == 3 {
                let wait = scope
                    .budget
                    .as_ref()
                    .map_or(ZERO_LIMIT_RETRY_AFTER, |budget| {
                        budget_wait(budget.window_secs, now / 1000)
                    });
                Err(RateRefusal::Budget(wait))
            } else if refusal == 4 {
                Err(RateRefusal::Unavailable)
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

        async fn observe_usage(
            &self,
            scope: &TrustedPolicyScope,
            attempt_id: uuid::Uuid,
            cost_nanos: Option<u64>,
        ) -> Result<(), ()> {
            self.settle_budget(scope, attempt_id, cost_nanos, false)
                .await
        }

        async fn finish(
            &self,
            scope: &TrustedPolicyScope,
            attempt_id: uuid::Uuid,
            outcome: AttemptOutcome,
        ) -> Result<(), ()> {
            self.settle_budget(
                scope,
                attempt_id,
                None,
                crate::gateway::budget::may_finalize(outcome),
            )
            .await
        }

        #[cfg(test)]
        async fn budget_total(&self, _scope: &TrustedPolicyScope) -> Option<u64> {
            None
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
            version: 1,
            tenant_key: format!("test-{name}"),
            rpm,
            tpm,
            budget: None,
        }
    }

    fn budget_scope(name: &str, limit_nanos: u64) -> TrustedPolicyScope {
        TrustedPolicyScope {
            version: 1,
            tenant_key: format!("test-{name}"),
            rpm: None,
            tpm: None,
            budget: Some(crate::gateway::budget::BudgetPolicySnapshot {
                limit_nanos,
                window_secs: 60,
                allow_unpriced: false,
                valid: true,
            }),
        }
    }

    fn reservation(
        amount_nanos: u64,
        window_index: u64,
    ) -> crate::gateway::budget::BudgetReservation {
        crate::gateway::budget::BudgetReservation {
            amount_nanos,
            window_index,
            expires_at_epoch_secs: epoch_secs().saturating_add(120),
            bounded: true,
        }
    }

    fn total_for(state: &LocalRateState, scope: &TrustedPolicyScope, window_index: u64) -> u64 {
        state
            .budget_totals
            .get(&BudgetWindowKey {
                tenant_key: scope.tenant_key.clone(),
                window_secs: 60,
                window_index,
            })
            .copied()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn local_budget_refusal_is_atomic_with_rate_debits() {
        let rates = LocalAttemptRates::new(RateLimitConfig::with_global(Some(1), None));
        let denied_scope = budget_scope("denied-budget", 50);
        assert!(matches!(
            rates
                .acquire(
                    "provider",
                    &denied_scope,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(reservation(51, 10)),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));

        assert!(rates
            .acquire(
                "provider",
                &scope("allowed-after-budget-refusal", None, None),
                1,
                uuid::Uuid::new_v4(),
                None,
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn local_budget_reservations_are_concurrent_and_window_owned() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let tenant = budget_scope("window-owner", 100);
        assert!(rates
            .acquire(
                "provider",
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(reservation(60, 10)),
            )
            .await
            .is_ok());
        assert!(matches!(
            rates
                .acquire(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(reservation(50, 10)),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
        assert!(rates
            .acquire(
                "provider",
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(reservation(50, 11)),
            )
            .await
            .is_ok());
        let state = rates.state.lock().await;
        assert_eq!(total_for(&state, &tenant, 10), 60);
        assert_eq!(total_for(&state, &tenant, 11), 50);
    }

    #[tokio::test]
    async fn cumulative_usage_settles_once_and_partial_abandonment_retains_liability() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let tenant = budget_scope("cumulative", 100);
        let settled_attempt = uuid::Uuid::new_v4();
        rates
            .acquire(
                "provider",
                &tenant,
                1,
                settled_attempt,
                Some(reservation(80, 20)),
            )
            .await
            .unwrap();
        for observed in [Some(20), Some(20), Some(30)] {
            rates
                .observe_usage(&tenant, settled_attempt, observed)
                .await
                .unwrap();
        }
        rates
            .finish(
                &tenant,
                settled_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        rates
            .finish(
                &tenant,
                settled_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 20), 30);
        }

        let abandoned_attempt = uuid::Uuid::new_v4();
        rates
            .acquire(
                "provider",
                &tenant,
                1,
                abandoned_attempt,
                Some(reservation(70, 20)),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, abandoned_attempt, Some(10))
            .await
            .unwrap();
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 20), 100);
        }
        assert!(matches!(
            rates
                .acquire(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(reservation(1, 20)),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[tokio::test]
    async fn unbounded_opt_in_still_records_known_charges_and_exhausts_the_cap() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let mut tenant = budget_scope("unbounded-known", 100);
        tenant.budget.as_mut().unwrap().allow_unpriced = true;
        let window_index = 30;
        for observed in [60, 50] {
            let attempt_id = uuid::Uuid::new_v4();
            let mut unbounded = reservation(0, window_index);
            unbounded.bounded = false;
            rates
                .acquire("provider", &tenant, 1, attempt_id, Some(unbounded))
                .await
                .unwrap();
            rates
                .observe_usage(&tenant, attempt_id, Some(observed))
                .await
                .unwrap();
            rates
                .finish(
                    &tenant,
                    attempt_id,
                    AttemptOutcome::Completed {
                        accounting: crate::policy::AttemptAccounting::UsageObserved,
                    },
                )
                .await
                .unwrap();
        }
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, window_index), 110);
        }
        let mut unbounded = reservation(0, window_index);
        unbounded.bounded = false;
        assert!(matches!(
            rates
                .acquire(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(unbounded),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[tokio::test]
    async fn local_provider_and_tenant_dimensions_are_each_enforced() {
        let provider_rates = LocalAttemptRates::new(RateLimitConfig::with_global(Some(1), None));
        let unlimited_tenant = scope("provider", None, None);
        assert!(provider_rates
            .acquire("provider", &unlimited_tenant, 1, uuid::Uuid::new_v4(), None)
            .await
            .is_ok());
        assert!(matches!(
            provider_rates
                .acquire("provider", &unlimited_tenant, 1, uuid::Uuid::new_v4(), None)
                .await,
            Err(RateRefusal::Provider(_))
        ));

        let tenant_rates = LocalAttemptRates::new(RateLimitConfig::default());
        let limited_tenant = scope("tenant", Some(1), None);
        assert!(tenant_rates
            .acquire("provider", &limited_tenant, 1, uuid::Uuid::new_v4(), None)
            .await
            .is_ok());
        assert!(matches!(
            tenant_rates
                .acquire("provider", &limited_tenant, 1, uuid::Uuid::new_v4(), None)
                .await,
            Err(RateRefusal::Tenant(_))
        ));
    }

    #[tokio::test]
    async fn local_tenant_denial_does_not_consume_global_allowance() {
        let rates = LocalAttemptRates::new(RateLimitConfig::with_global(Some(1), None));
        let denied_tenant = scope("denied", None, Some(1));
        assert!(matches!(
            rates
                .acquire("provider", &denied_tenant, 2, uuid::Uuid::new_v4(), None)
                .await,
            Err(RateRefusal::Tenant(_))
        ));
        assert!(rates
            .acquire(
                "provider",
                &scope("allowed", None, None),
                1,
                uuid::Uuid::new_v4(),
                None,
            )
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
        assert!(first
            .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), None)
            .await
            .is_ok());
        assert!(matches!(
            second
                .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), None)
                .await,
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
                .acquire(
                    &provider,
                    &scope("denied", None, Some(1)),
                    2,
                    uuid::Uuid::new_v4(),
                    None,
                )
                .await,
            Err(RateRefusal::Tenant(_))
        ));
        assert!(rates
            .acquire(
                &provider,
                &scope("allowed", None, None),
                1,
                uuid::Uuid::new_v4(),
                None,
            )
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
            .acquire(
                &provider,
                &scope("tenant", None, None),
                1,
                uuid::Uuid::new_v4(),
                None,
            )
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

        assert!(rates
            .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), None)
            .await
            .is_ok());
        assert!(rates
            .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), None)
            .await
            .is_ok());
        assert!(matches!(
            rates
                .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), None)
                .await,
            Err(RateRefusal::Tenant(_))
        ));
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_budget_reservation_and_settlement_are_shared_across_origins() {
        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-budget-{}", uuid::Uuid::new_v4().simple());
        let tenant = budget_scope(&format!("redis-{}", uuid::Uuid::new_v4()), 100);
        let first = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let second = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let window_index = epoch_secs() / 60;
        let first_attempt = uuid::Uuid::new_v4();
        first
            .acquire(
                &provider,
                &tenant,
                1,
                first_attempt,
                Some(reservation(60, window_index)),
            )
            .await
            .unwrap();
        assert!(matches!(
            second
                .acquire(
                    &provider,
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(reservation(50, window_index)),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
        first
            .observe_usage(&tenant, first_attempt, Some(30))
            .await
            .unwrap();
        first
            .finish(
                &tenant,
                first_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        assert!(second
            .acquire(
                &provider,
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(reservation(70, window_index)),
            )
            .await
            .is_ok());
        assert!(second
            .acquire(
                &provider,
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(reservation(100, window_index + 1)),
            )
            .await
            .is_ok());
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_failed_settlement_keeps_the_acquired_liability() {
        use redis::AsyncCommands;

        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-settle-{}", uuid::Uuid::new_v4().simple());
        let tenant = budget_scope(&format!("redis-{}", uuid::Uuid::new_v4()), 100);
        let rates = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let window_index = epoch_secs() / 60;
        let attempt_id = uuid::Uuid::new_v4();
        rates
            .acquire(
                &provider,
                &tenant,
                1,
                attempt_id,
                Some(reservation(80, window_index)),
            )
            .await
            .unwrap();

        let mut connection = rates.connection().await.unwrap();
        let attempt_key = RedisAttemptRates::budget_attempt_key(&tenant, attempt_id);
        let total_key = RedisAttemptRates::budget_total_key(&tenant, window_index);
        let _: usize = connection.del(attempt_key).await.unwrap();
        assert!(rates
            .observe_usage(&tenant, attempt_id, Some(10))
            .await
            .is_err());
        let retained: u64 = connection.get(total_key).await.unwrap();
        assert_eq!(retained, 80);
        assert!(matches!(
            rates
                .acquire(
                    &provider,
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(reservation(21, window_index)),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }
}
