use crate::gateway::auth::Identity;
use crate::policy::{
    AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
    AttemptPolicyErrorKind, AttemptPolicyFuture, AttemptPolicyRefusal, AttemptPolicyRefusalKind,
    AttemptUsageObservation, DispatchPolicyContext, PreparedAttempt,
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
const LATE_SETTLEMENT_RETENTION_SECS: u64 = 86_400;

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
        quote: Option<crate::gateway::budget::BudgetQuote>,
    ) -> Result<(), RateRefusal>;

    async fn penalize(&self, provider: &str, duration: Duration) -> Result<(), ()>;

    async fn observe_usage(
        &self,
        scope: &TrustedPolicyScope,
        attempt_id: uuid::Uuid,
        observation: crate::gateway::budget::SpendObservation,
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
    budget_frozen: std::collections::HashSet<BudgetWindowKey>,
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
    pricing_bounded: bool,
    liability_nanos: u64,
    observed_nanos: Option<u64>,
    max_provider_nanos: Option<u64>,
    terminal_catalog_nanos: Option<u64>,
    terminal_provider_nanos: Option<u64>,
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
                budget_frozen: std::collections::HashSet::new(),
                budget_attempts: HashMap::new(),
            }),
        }
    }

    async fn acquire_at_epoch(
        &self,
        provider: &str,
        scope: &TrustedPolicyScope,
        permits: u32,
        attempt_id: uuid::Uuid,
        quote: Option<crate::gateway::budget::BudgetQuote>,
        test_epoch_secs: Option<u64>,
    ) -> Result<(), RateRefusal> {
        let global_limit = self.config.resolve(provider);
        let tenant_key = format!("{}:{provider}", scope.tenant_key);
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let now_epoch_secs = test_epoch_secs.unwrap_or_else(epoch_secs);
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

        let budget_commit = match (&scope.budget, quote) {
            (None, None) => None,
            (Some(policy), Some(mut quote)) => {
                if state.budget_attempts.contains_key(&attempt_id) {
                    return Err(RateRefusal::Unavailable);
                }
                let window_index = now_epoch_secs / policy.window_secs;
                let window_key = BudgetWindowKey {
                    tenant_key: scope.tenant_key.clone(),
                    window_secs: policy.window_secs,
                    window_index,
                };
                let current = state.budget_totals.get(&window_key).copied().unwrap_or(0);
                let admitted = !state.budget_frozen.contains(&window_key)
                    && policy.limit_nanos != 0
                    && if quote.bounded {
                        current
                            .checked_add(quote.amount_nanos)
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
                if !quote.bounded {
                    quote.amount_nanos = policy.limit_nanos - current;
                }
                let next_window = window_index
                    .saturating_add(1)
                    .saturating_mul(policy.window_secs);
                Some((
                    window_key,
                    quote,
                    next_window.saturating_add(LATE_SETTLEMENT_RETENTION_SECS),
                ))
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
        if let Some((window_key, quote, expires_at_epoch_secs)) = budget_commit {
            let total = state.budget_totals.entry(window_key.clone()).or_insert(0);
            *total = total
                .checked_add(quote.amount_nanos)
                .ok_or(RateRefusal::Unavailable)?;
            state.budget_attempts.insert(
                attempt_id,
                AttemptLiability {
                    window_key,
                    pricing_bounded: quote.bounded,
                    liability_nanos: quote.amount_nanos,
                    observed_nanos: None,
                    max_provider_nanos: None,
                    terminal_catalog_nanos: None,
                    terminal_provider_nanos: None,
                    finalized: false,
                    expires_at_epoch_secs,
                },
            );
        }
        Ok(())
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
    state
        .budget_frozen
        .retain(|window| active_windows.contains(window));
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
        quote: Option<crate::gateway::budget::BudgetQuote>,
    ) -> Result<(), RateRefusal> {
        self.acquire_at_epoch(provider, scope, permits, attempt_id, quote, None)
            .await
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
        mut observation: crate::gateway::budget::SpendObservation,
    ) -> Result<(), ()> {
        if scope.budget.is_none() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let increase = {
            let attempt = state.budget_attempts.get_mut(&attempt_id).ok_or(())?;
            if attempt.finalized {
                return Ok(());
            }
            if !attempt.pricing_bounded
                && observation.authority == crate::gateway::budget::SpendAuthority::Catalog
            {
                observation.terminal_authoritative = false;
            }
            if observation.terminal_authoritative {
                if let Some(amount_nanos) = observation.amount_nanos {
                    match observation.authority {
                        crate::gateway::budget::SpendAuthority::Provider => {
                            attempt.terminal_provider_nanos = Some(amount_nanos);
                        }
                        crate::gateway::budget::SpendAuthority::Catalog => {
                            attempt.terminal_catalog_nanos = Some(
                                attempt
                                    .terminal_catalog_nanos
                                    .map_or(amount_nanos, |current| current.max(amount_nanos)),
                            );
                        }
                    }
                }
            }
            if observation.authority == crate::gateway::budget::SpendAuthority::Provider {
                if let Some(amount_nanos) = observation.amount_nanos {
                    attempt.max_provider_nanos = Some(
                        attempt
                            .max_provider_nanos
                            .map_or(amount_nanos, |current| current.max(amount_nanos)),
                    );
                }
            }
            match observation.amount_nanos {
                Some(amount_nanos) if amount_nanos > attempt.observed_nanos.unwrap_or_default() => {
                    attempt.observed_nanos = Some(amount_nanos);
                    if amount_nanos > attempt.liability_nanos {
                        let delta = amount_nanos - attempt.liability_nanos;
                        attempt.liability_nanos = amount_nanos;
                        Some((attempt.window_key.clone(), delta))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        if let Some((window_key, delta)) = increase {
            let total = state.budget_totals.get_mut(&window_key).ok_or(())?;
            match total.checked_add(delta) {
                Some(next) if next <= crate::gateway::budget::MAX_EXACT_REDIS_NANOS => {
                    *total = next;
                }
                _ => {
                    *total = crate::gateway::budget::MAX_EXACT_REDIS_NANOS;
                    state.budget_frozen.insert(window_key);
                }
            }
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
        let settlement = {
            let attempt = state.budget_attempts.get_mut(&attempt_id).ok_or(())?;
            if attempt.finalized {
                return Ok(());
            }
            if crate::gateway::budget::may_finalize(outcome) {
                let final_nanos = attempt.terminal_provider_nanos.or_else(|| {
                    attempt.terminal_catalog_nanos.map(|catalog_nanos| {
                        catalog_nanos.max(attempt.max_provider_nanos.unwrap_or_default())
                    })
                });
                if let Some(final_nanos) = final_nanos {
                    let previous_liability = attempt.liability_nanos;
                    attempt.liability_nanos = final_nanos;
                    attempt.finalized = true;
                    Some((attempt.window_key.clone(), previous_liability, final_nanos))
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some((window_key, previous_liability, final_nanos)) = settlement {
            if state.budget_frozen.contains(&window_key) {
                return Ok(());
            }
            let total = state.budget_totals.get_mut(&window_key).ok_or(())?;
            if final_nanos <= previous_liability {
                *total = total
                    .checked_sub(previous_liability - final_nanos)
                    .ok_or(())?;
            } else {
                let delta = final_nanos - previous_liability;
                match total.checked_add(delta) {
                    Some(next) if next <= crate::gateway::budget::MAX_EXACT_REDIS_NANOS => {
                        *total = next;
                    }
                    _ => {
                        *total = crate::gateway::budget::MAX_EXACT_REDIS_NANOS;
                        state.budget_frozen.insert(window_key);
                    }
                }
            }
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
        let quote = crate::gateway::budget::quote(scope.budget.as_ref(), attempt)
            .map_err(|_| AttemptPolicyRefusal::new(AttemptPolicyRefusalKind::Unpriceable, None))?;
        let permits = crate::proxy::ratelimit::estimate_attempt_tokens(attempt);
        match self
            .rates
            .acquire(provider, scope, permits, attempt.identity().id(), quote)
            .await
        {
            Ok(()) => {
                if quote.is_some_and(|quote| !quote.bounded) {
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
        _quote: Option<crate::gateway::budget::BudgetQuote>,
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
        _observation: crate::gateway::budget::SpendObservation,
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
                        crate::gateway::budget::spend_observation(AttemptUsageObservation::new(
                            usage, false, false, false,
                        )),
                    )
                    .await
                    .map_err(|_| {
                        AttemptPolicyError::new(AttemptPolicyErrorKind::CoordinatorUnavailable)
                    }),
                AttemptEvent::ResponseHeaders { .. } => Ok(()),
            }
        })
    }

    fn observe_usage<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        observation: AttemptUsageObservation<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            self.coordinator
                .rates
                .observe_usage(
                    &self.scope,
                    attempt.id(),
                    crate::gateway::budget::spend_observation(observation),
                )
                .await
                .map_err(|_| {
                    AttemptPolicyError::new(AttemptPolicyErrorKind::CoordinatorUnavailable)
                })
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
        local redis_time = redis.call('TIME')
        local now = tonumber(redis_time[1]) * 1000 + math.floor(tonumber(redis_time[2]) / 1000)
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
        local budget_total_key = KEYS[5]
        local budget_freeze_key = KEYS[5] .. 'frozen'
        local budget_ttl = 1
        if budget_enabled then
            local bounded = tonumber(ARGV[20]) == 1
            local budget_limit = tonumber(ARGV[21])
            reservation = tonumber(ARGV[22])
            local window_secs = tonumber(ARGV[23])
            local retention_secs = tonumber(ARGV[24])
            local epoch_secs = math.floor(now / 1000)
            local window_index = math.floor(epoch_secs / window_secs)
            budget_total_key = KEYS[5] .. window_index
            budget_freeze_key = budget_total_key .. ':frozen'
            budget_ttl = math.max(1,
                ((window_index + 1) * window_secs + retention_secs) * 1000 - now)
            local current = tonumber(redis.call('GET', budget_total_key)) or 0
            if redis.call('EXISTS', KEYS[6]) == 1 then
                admitted = 0
                refusal = 4
            elseif redis.call('EXISTS', budget_freeze_key) == 1 then
                admitted = 0
                refusal = 3
                wait = math.max(wait, ((window_index + 1) * window_secs * 1000) - now)
            elseif budget_limit == 0 then
                admitted = 0
                refusal = 3
                wait = math.max(wait, ((window_index + 1) * window_secs * 1000) - now)
            elseif bounded then
                if reservation > budget_limit or current > budget_limit - reservation then
                    admitted = 0
                    refusal = 3
                    wait = math.max(wait, ((window_index + 1) * window_secs * 1000) - now)
                else
                    budget_next = current + reservation
                end
            elseif current >= budget_limit then
                admitted = 0
                refusal = 3
                wait = math.max(wait, ((window_index + 1) * window_secs * 1000) - now)
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
                redis.call('SET', budget_total_key, budget_next, 'PX', budget_ttl)
                redis.call('HSET', KEYS[6],
                    'liability', reservation,
                    'bounded', ARGV[20],
                    'observed', -1,
                    'finalized', 0,
                    'total_key', budget_total_key,
                    'freeze_key', budget_freeze_key,
                    'provider_floor', -1,
                    'terminal_catalog', -1,
                    'terminal_provider', -1)
                redis.call('PEXPIRE', KEYS[6], budget_ttl)
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
        local values = redis.call('HMGET', KEYS[1],
            'liability', 'bounded', 'observed', 'finalized', 'total_key',
            'freeze_key', 'provider_floor', 'terminal_catalog', 'terminal_provider')
        local liability = tonumber(values[1]) or 0
        local bounded = tonumber(values[2]) or 0
        local observed = tonumber(values[3]) or -1
        local finalized = tonumber(values[4]) or 0
        local total_key = values[5]
        local freeze_key = values[6]
        local provider_floor = tonumber(values[7]) or -1
        local terminal_catalog = tonumber(values[8]) or -1
        local terminal_provider = tonumber(values[9]) or -1
        if finalized == 1 then return 1 end
        local incoming = tonumber(ARGV[1])
        local should_finalize = tonumber(ARGV[2]) == 1
        local max_exact = tonumber(ARGV[3])
        local authority = tonumber(ARGV[4])
        local terminal_authoritative = tonumber(ARGV[5]) == 1
        if bounded == 0 and authority ~= 1 then
            terminal_authoritative = false
        end
        if authority == 1 and incoming > provider_floor then
            provider_floor = incoming
            redis.call('HSET', KEYS[1], 'provider_floor', incoming)
        end
        if terminal_authoritative and incoming >= 0 then
            if authority == 1 then
                terminal_provider = incoming
                redis.call('HSET', KEYS[1], 'terminal_provider', incoming)
            elseif authority == 0 and incoming > terminal_catalog then
                terminal_catalog = incoming
                redis.call('HSET', KEYS[1], 'terminal_catalog', incoming)
            end
        end
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
                local ttl = redis.call('PTTL', total_key)
                redis.call('SET', freeze_key, 1, 'PX', math.max(1, ttl))
            else
                total = total + delta
                liability = observed
            end
            redis.call('SET', total_key, total, 'KEEPTTL')
            redis.call('HSET', KEYS[1], 'liability', liability)
        end
        if should_finalize then
            local final = terminal_provider
            if final < 0 and terminal_catalog >= 0 then
                final = math.max(terminal_catalog, provider_floor)
            end
            if final >= 0 and redis.call('EXISTS', freeze_key) == 0 then
                local total = tonumber(redis.call('GET', total_key)) or 0
                if final < liability then
                    total = math.max(0, total - (liability - final))
                elseif final > liability then
                    local delta = final - liability
                    if total > max_exact - delta then
                        total = max_exact
                        local ttl = redis.call('PTTL', total_key)
                        redis.call('SET', freeze_key, 1, 'PX', math.max(1, ttl))
                    else
                        total = total + delta
                    end
                end
                redis.call('SET', total_key, total, 'KEEPTTL')
            end
            if final >= 0 then
                liability = final
                redis.call('HSET', KEYS[1], 'liability', liability, 'finalized', 1)
            end
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

        fn budget_total_prefix(scope: &TrustedPolicyScope) -> String {
            format!(
                "llmshim:gw:budget:v1:{}:{}:",
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
            observation: Option<crate::gateway::budget::SpendObservation>,
            finalize: bool,
        ) -> Result<(), ()> {
            if scope.budget.is_none() {
                return Ok(());
            }
            let mut connection = self.connection().await.map_err(|_| ())?;
            let result: i64 = self
                .settle_budget_script
                .key(Self::budget_attempt_key(scope, attempt_id))
                .arg(
                    observation
                        .and_then(|value| value.amount_nanos)
                        .map_or(-1_i64, |value| value as i64),
                )
                .arg(i32::from(finalize))
                .arg(crate::gateway::budget::MAX_EXACT_REDIS_NANOS)
                .arg(observation.map_or(-1_i32, |value| match value.authority {
                    crate::gateway::budget::SpendAuthority::Catalog => 0,
                    crate::gateway::budget::SpendAuthority::Provider => 1,
                }))
                .arg(i32::from(
                    observation.is_some_and(|value| value.terminal_authoritative),
                ))
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
            quote: Option<crate::gateway::budget::BudgetQuote>,
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
            let mut invocation = self.combined_script.prepare_invoke();
            invocation
                .key(Self::global_key(provider, "rpm"))
                .key(Self::global_key(provider, "tpm"))
                .key(Self::tenant_key(scope, provider, "rpm"))
                .key(Self::tenant_key(scope, provider, "tpm"));
            match (&scope.budget, quote) {
                (Some(_), Some(_)) => {
                    invocation
                        .key(Self::budget_total_prefix(scope))
                        .key(Self::budget_attempt_key(scope, attempt_id));
                }
                (None, None) => {
                    invocation
                        .key("llmshim:gw:budget:v1:none")
                        .key(format!("llmshim:gw:budget:v1:none:{attempt_id}"));
                }
                _ => return Err(RateRefusal::Unavailable),
            }
            invocation.arg(0).arg(KEY_TTL_MS);
            Self::add_dimension(&mut invocation, global.rpm, 1.0);
            Self::add_dimension(&mut invocation, global.tpm, permits.max(1) as f64);
            Self::add_dimension(&mut invocation, scope.rpm, 1.0);
            Self::add_dimension(&mut invocation, scope.tpm, permits.max(1) as f64);
            match (&scope.budget, quote) {
                (Some(policy), Some(quote)) => {
                    invocation
                        .arg(1)
                        .arg(i32::from(quote.bounded))
                        .arg(policy.limit_nanos)
                        .arg(quote.amount_nanos)
                        .arg(policy.window_secs)
                        .arg(LATE_SETTLEMENT_RETENTION_SECS);
                }
                (None, None) => {
                    invocation.arg(0).arg(0).arg(0).arg(0).arg(1).arg(1);
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
                Err(RateRefusal::Budget(Duration::from_millis(
                    wait_ms.max(1) as u64
                )))
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
            observation: crate::gateway::budget::SpendObservation,
        ) -> Result<(), ()> {
            self.settle_budget(scope, attempt_id, Some(observation), false)
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

    fn quote(amount_nanos: u64) -> crate::gateway::budget::BudgetQuote {
        crate::gateway::budget::BudgetQuote {
            amount_nanos,
            bounded: true,
        }
    }

    fn spend_observation(
        amount_nanos: Option<u64>,
        authority: crate::gateway::budget::SpendAuthority,
        terminal_authoritative: bool,
    ) -> crate::gateway::budget::SpendObservation {
        crate::gateway::budget::SpendObservation {
            amount_nanos,
            authority,
            terminal_authoritative,
        }
    }

    fn inferred_terminal_zero() -> crate::gateway::budget::SpendObservation {
        crate::gateway::budget::SpendObservation {
            amount_nanos: Some(0),
            authority: crate::gateway::budget::SpendAuthority::Catalog,
            terminal_authoritative: true,
        }
    }

    fn partial_catalog(amount_nanos: u64) -> crate::gateway::budget::SpendObservation {
        spend_observation(
            Some(amount_nanos),
            crate::gateway::budget::SpendAuthority::Catalog,
            false,
        )
    }

    fn terminal_catalog(amount_nanos: u64) -> crate::gateway::budget::SpendObservation {
        spend_observation(
            Some(amount_nanos),
            crate::gateway::budget::SpendAuthority::Catalog,
            true,
        )
    }

    fn terminal_provider(amount_nanos: u64) -> crate::gateway::budget::SpendObservation {
        spend_observation(
            Some(amount_nanos),
            crate::gateway::budget::SpendAuthority::Provider,
            true,
        )
    }

    fn partial_provider(amount_nanos: u64) -> crate::gateway::budget::SpendObservation {
        spend_observation(
            Some(amount_nanos),
            crate::gateway::budget::SpendAuthority::Provider,
            false,
        )
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
                    Some(quote(51)),
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
    async fn zero_budget_freezes_even_a_zero_cost_quote_without_rate_debit() {
        let rates = LocalAttemptRates::new(RateLimitConfig::with_global(Some(1), None));
        let frozen = budget_scope("zero-budget", 0);
        assert!(matches!(
            rates
                .acquire("provider", &frozen, 1, uuid::Uuid::new_v4(), Some(quote(0)))
                .await,
            Err(RateRefusal::Budget(_))
        ));
        assert!(rates
            .acquire(
                "provider",
                &scope("after-zero-budget", None, None),
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
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(quote(60)),
                Some(600),
            )
            .await
            .is_ok());
        assert!(matches!(
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(quote(50)),
                    Some(600),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
        assert!(rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(quote(50)),
                Some(660),
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
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                settled_attempt,
                Some(quote(80)),
                Some(1200),
            )
            .await
            .unwrap();
        for observed in [
            partial_catalog(20),
            partial_catalog(20),
            terminal_catalog(30),
        ] {
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
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                abandoned_attempt,
                Some(quote(70)),
                Some(1200),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, abandoned_attempt, partial_catalog(10))
            .await
            .unwrap();
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 20), 100);
        }
        assert!(matches!(
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(quote(1)),
                    Some(1200),
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
            let mut unbounded = quote(0);
            unbounded.bounded = false;
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    attempt_id,
                    Some(unbounded),
                    Some(window_index * 60),
                )
                .await
                .unwrap();
            rates
                .observe_usage(&tenant, attempt_id, terminal_provider(observed))
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
        let mut unbounded = quote(0);
        unbounded.bounded = false;
        assert!(matches!(
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(unbounded),
                    Some(window_index * 60),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[tokio::test]
    async fn oversized_known_cost_saturates_and_duplicate_settlement_is_idempotent() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let tenant = budget_scope(
            "known-overflow",
            crate::gateway::budget::MAX_EXACT_REDIS_NANOS,
        );
        let attempt_id = uuid::Uuid::new_v4();
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                attempt_id,
                Some(quote(10)),
                Some(2400),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            rates
                .observe_usage(
                    &tenant,
                    attempt_id,
                    terminal_provider(crate::gateway::budget::MAX_EXACT_REDIS_NANOS),
                )
                .await
                .unwrap();
        }
        for _ in 0..2 {
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
            assert_eq!(
                total_for(&state, &tenant, 40),
                crate::gateway::budget::MAX_EXACT_REDIS_NANOS
            );
        }
        assert!(matches!(
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(quote(1)),
                    Some(2400),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[tokio::test]
    async fn saturated_window_stays_frozen_when_an_overlapping_attempt_releases() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let tenant = budget_scope(
            "overlap-overflow",
            crate::gateway::budget::MAX_EXACT_REDIS_NANOS,
        );
        let overflowing_attempt = uuid::Uuid::new_v4();
        let releasing_attempt = uuid::Uuid::new_v4();
        for attempt_id in [overflowing_attempt, releasing_attempt] {
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    attempt_id,
                    Some(quote(10)),
                    Some(2460),
                )
                .await
                .unwrap();
        }
        rates
            .observe_usage(
                &tenant,
                overflowing_attempt,
                terminal_provider(crate::gateway::budget::MAX_EXACT_REDIS_NANOS),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            rates
                .observe_usage(&tenant, releasing_attempt, terminal_provider(0))
                .await
                .unwrap();
            rates
                .finish(
                    &tenant,
                    releasing_attempt,
                    AttemptOutcome::Completed {
                        accounting: crate::policy::AttemptAccounting::UsageObserved,
                    },
                )
                .await
                .unwrap();
        }
        {
            let state = rates.state.lock().await;
            assert_eq!(
                total_for(&state, &tenant, 41),
                crate::gateway::budget::MAX_EXACT_REDIS_NANOS
            );
            assert!(state.budget_frozen.contains(&BudgetWindowKey {
                tenant_key: tenant.tenant_key.clone(),
                window_secs: 60,
                window_index: 41,
            }));
        }
        assert!(matches!(
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(quote(1)),
                    Some(2460),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[tokio::test]
    async fn partial_usage_cannot_release_but_terminal_provider_bill_can_correct_catalog() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let tenant = budget_scope("partial-terminal", 100);
        let partial_attempt = uuid::Uuid::new_v4();
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                partial_attempt,
                Some(quote(80)),
                Some(2520),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, partial_attempt, partial_catalog(10))
            .await
            .unwrap();
        rates
            .finish(
                &tenant,
                partial_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 42), 80);
        }

        let correction_attempt = uuid::Uuid::new_v4();
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                correction_attempt,
                Some(quote(20)),
                Some(2580),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, correction_attempt, partial_catalog(80))
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, correction_attempt, terminal_provider(15))
            .await
            .unwrap();
        rates
            .finish(
                &tenant,
                correction_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 43), 15);
        }

        let zero_attempt = uuid::Uuid::new_v4();
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                zero_attempt,
                Some(quote(80)),
                Some(2640),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, zero_attempt, inferred_terminal_zero())
            .await
            .unwrap();
        rates
            .finish(
                &tenant,
                zero_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 44), 0);
        }
    }

    #[tokio::test]
    async fn unpriced_explicit_zero_counters_do_not_release_fee_uncertainty() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let mut tenant = budget_scope("unpriced-zero", 100);
        tenant.budget.as_mut().unwrap().allow_unpriced = true;
        let attempt_id = uuid::Uuid::new_v4();
        let mut unbounded_quote = quote(0);
        unbounded_quote.bounded = false;
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                attempt_id,
                Some(unbounded_quote),
                Some(2700),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, attempt_id, inferred_terminal_zero())
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
        {
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, 45), 100);
        }
        assert!(matches!(
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    uuid::Uuid::new_v4(),
                    Some(quote(1)),
                    Some(2700),
                )
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[tokio::test]
    async fn terminal_catalog_cannot_release_below_partial_provider_floor() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let tenant = budget_scope("provider-floor", 100);
        for (window_index, outcome) in [
            (
                46,
                Some(AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                }),
            ),
            (
                47,
                Some(AttemptOutcome::StreamFailure {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                }),
            ),
            (48, None),
        ] {
            let attempt_id = uuid::Uuid::new_v4();
            rates
                .acquire_at_epoch(
                    "provider",
                    &tenant,
                    1,
                    attempt_id,
                    Some(quote(20)),
                    Some(window_index * 60),
                )
                .await
                .unwrap();
            for _ in 0..2 {
                rates
                    .observe_usage(&tenant, attempt_id, partial_provider(80))
                    .await
                    .unwrap();
            }
            rates
                .observe_usage(&tenant, attempt_id, terminal_catalog(15))
                .await
                .unwrap();
            if let Some(outcome) = outcome {
                rates.finish(&tenant, attempt_id, outcome).await.unwrap();
            }
            let state = rates.state.lock().await;
            assert_eq!(total_for(&state, &tenant, window_index), 80);
            drop(state);
            assert!(matches!(
                rates
                    .acquire_at_epoch(
                        "provider",
                        &tenant,
                        1,
                        uuid::Uuid::new_v4(),
                        Some(quote(21)),
                        Some(window_index * 60),
                    )
                    .await,
                Err(RateRefusal::Budget(_))
            ));
        }
    }

    #[tokio::test]
    async fn late_settlement_keeps_the_acquisition_window_tombstone() {
        let rates = LocalAttemptRates::new(RateLimitConfig::default());
        let mut tenant = budget_scope("late-settlement", 100);
        tenant.budget.as_mut().unwrap().window_secs = 1;
        let old_attempt = uuid::Uuid::new_v4();
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                old_attempt,
                Some(quote(80)),
                Some(10),
            )
            .await
            .unwrap();
        rates
            .acquire_at_epoch(
                "provider",
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(quote(100)),
                Some(13),
            )
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, old_attempt, terminal_catalog(25))
            .await
            .unwrap();
        rates
            .finish(
                &tenant,
                old_attempt,
                AttemptOutcome::Completed {
                    accounting: crate::policy::AttemptAccounting::UsageObserved,
                },
            )
            .await
            .unwrap();
        let state = rates.state.lock().await;
        let old_window = BudgetWindowKey {
            tenant_key: tenant.tenant_key.clone(),
            window_secs: 1,
            window_index: 10,
        };
        assert_eq!(state.budget_totals.get(&old_window), Some(&25));
        assert!(state
            .budget_attempts
            .get(&old_attempt)
            .is_some_and(|attempt| attempt.finalized));
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
        let mut tenant = budget_scope(&format!("redis-{}", uuid::Uuid::new_v4()), 100);
        tenant.budget.as_mut().unwrap().window_secs = 1;
        let first = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let second = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let frozen = budget_scope(&format!("redis-zero-{}", uuid::Uuid::new_v4()), 0);
        assert!(matches!(
            first
                .acquire(&provider, &frozen, 1, uuid::Uuid::new_v4(), Some(quote(0)),)
                .await,
            Err(RateRefusal::Budget(_))
        ));
        let first_attempt = uuid::Uuid::new_v4();
        first
            .acquire(&provider, &tenant, 1, first_attempt, Some(quote(60)))
            .await
            .unwrap();
        assert!(matches!(
            second
                .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), Some(quote(50)),)
                .await,
            Err(RateRefusal::Budget(_))
        ));
        first
            .observe_usage(&tenant, first_attempt, terminal_catalog(30))
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
            .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), Some(quote(70)),)
            .await
            .is_ok());
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(second
            .acquire(
                &provider,
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(quote(100)),
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
        let attempt_id = uuid::Uuid::new_v4();
        rates
            .acquire(&provider, &tenant, 1, attempt_id, Some(quote(80)))
            .await
            .unwrap();

        let mut connection = rates.connection().await.unwrap();
        let attempt_key = RedisAttemptRates::budget_attempt_key(&tenant, attempt_id);
        let total_key: String = connection.hget(&attempt_key, "total_key").await.unwrap();
        let _: usize = connection.del(attempt_key).await.unwrap();
        assert!(rates
            .observe_usage(&tenant, attempt_id, partial_catalog(10))
            .await
            .is_err());
        let retained: u64 = connection.get(total_key).await.unwrap();
        assert_eq!(retained, 80);
        assert!(matches!(
            rates
                .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), Some(quote(21)),)
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_late_oversized_settlement_saturates_and_keeps_its_tombstone() {
        use redis::AsyncCommands;

        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-late-{}", uuid::Uuid::new_v4().simple());
        let mut tenant = budget_scope(
            &format!("redis-{}", uuid::Uuid::new_v4()),
            crate::gateway::budget::MAX_EXACT_REDIS_NANOS,
        );
        tenant.budget.as_mut().unwrap().window_secs = 1;
        let rates = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let attempt_id = uuid::Uuid::new_v4();
        rates
            .acquire(&provider, &tenant, 1, attempt_id, Some(quote(10)))
            .await
            .unwrap();
        let mut connection = rates.connection().await.unwrap();
        let attempt_key = RedisAttemptRates::budget_attempt_key(&tenant, attempt_id);
        let total_key: String = connection.hget(&attempt_key, "total_key").await.unwrap();

        tokio::time::sleep(Duration::from_millis(1_100)).await;
        for _ in 0..2 {
            rates
                .observe_usage(
                    &tenant,
                    attempt_id,
                    terminal_provider(crate::gateway::budget::MAX_EXACT_REDIS_NANOS),
                )
                .await
                .unwrap();
        }
        for _ in 0..2 {
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
        let retained: u64 = connection.get(&total_key).await.unwrap();
        assert_eq!(retained, crate::gateway::budget::MAX_EXACT_REDIS_NANOS);
        let tombstone_exists: bool = connection.exists(&attempt_key).await.unwrap();
        assert!(tombstone_exists);
        assert!(rates
            .acquire(
                &provider,
                &tenant,
                1,
                uuid::Uuid::new_v4(),
                Some(quote(100)),
            )
            .await
            .is_ok());
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_saturated_window_cannot_be_reopened_by_overlapping_release() {
        use redis::AsyncCommands;

        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-overlap-{}", uuid::Uuid::new_v4().simple());
        let tenant = budget_scope(
            &format!("redis-{}", uuid::Uuid::new_v4()),
            crate::gateway::budget::MAX_EXACT_REDIS_NANOS,
        );
        let rates = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let overflowing_attempt = uuid::Uuid::new_v4();
        let releasing_attempt = uuid::Uuid::new_v4();
        for attempt_id in [overflowing_attempt, releasing_attempt] {
            rates
                .acquire(&provider, &tenant, 1, attempt_id, Some(quote(10)))
                .await
                .unwrap();
        }
        rates
            .observe_usage(
                &tenant,
                overflowing_attempt,
                terminal_provider(crate::gateway::budget::MAX_EXACT_REDIS_NANOS),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            rates
                .observe_usage(&tenant, releasing_attempt, terminal_provider(0))
                .await
                .unwrap();
            rates
                .finish(
                    &tenant,
                    releasing_attempt,
                    AttemptOutcome::Completed {
                        accounting: crate::policy::AttemptAccounting::UsageObserved,
                    },
                )
                .await
                .unwrap();
        }

        let mut connection = rates.connection().await.unwrap();
        let attempt_key = RedisAttemptRates::budget_attempt_key(&tenant, overflowing_attempt);
        let total_key: String = connection.hget(&attempt_key, "total_key").await.unwrap();
        let freeze_key: String = connection.hget(&attempt_key, "freeze_key").await.unwrap();
        let retained: u64 = connection.get(total_key).await.unwrap();
        let frozen: bool = connection.exists(freeze_key).await.unwrap();
        assert_eq!(retained, crate::gateway::budget::MAX_EXACT_REDIS_NANOS);
        assert!(frozen);
        assert!(matches!(
            rates
                .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), Some(quote(1)),)
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_terminal_provider_bill_replaces_higher_catalog_observation() {
        use redis::AsyncCommands;

        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-authority-{}", uuid::Uuid::new_v4().simple());
        let tenant = budget_scope(&format!("redis-{}", uuid::Uuid::new_v4()), 100);
        let rates = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let attempt_id = uuid::Uuid::new_v4();
        rates
            .acquire(&provider, &tenant, 1, attempt_id, Some(quote(20)))
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, attempt_id, partial_catalog(80))
            .await
            .unwrap();
        rates
            .observe_usage(&tenant, attempt_id, terminal_provider(15))
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

        let mut connection = rates.connection().await.unwrap();
        let attempt_key = RedisAttemptRates::budget_attempt_key(&tenant, attempt_id);
        let total_key: String = connection.hget(attempt_key, "total_key").await.unwrap();
        let retained: u64 = connection.get(total_key).await.unwrap();
        assert_eq!(retained, 15);
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_terminal_catalog_cannot_release_below_partial_provider_floor() {
        use redis::AsyncCommands;

        let Some(redis_url) = redis_url() else {
            return;
        };
        let provider = format!("attempt-floor-{}", uuid::Uuid::new_v4().simple());
        let tenant = budget_scope(&format!("redis-{}", uuid::Uuid::new_v4()), 100);
        let rates = RedisAttemptRates::new(&redis_url, RateLimitConfig::default()).unwrap();
        let attempt_id = uuid::Uuid::new_v4();
        rates
            .acquire(&provider, &tenant, 1, attempt_id, Some(quote(20)))
            .await
            .unwrap();
        for _ in 0..2 {
            rates
                .observe_usage(&tenant, attempt_id, partial_provider(80))
                .await
                .unwrap();
        }
        rates
            .observe_usage(&tenant, attempt_id, terminal_catalog(15))
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

        let mut connection = rates.connection().await.unwrap();
        let attempt_key = RedisAttemptRates::budget_attempt_key(&tenant, attempt_id);
        let total_key: String = connection.hget(attempt_key, "total_key").await.unwrap();
        let retained: u64 = connection.get(total_key).await.unwrap();
        assert_eq!(retained, 80);
        assert!(matches!(
            rates
                .acquire(&provider, &tenant, 1, uuid::Uuid::new_v4(), Some(quote(21)),)
                .await,
            Err(RateRefusal::Budget(_))
        ));
    }
}
